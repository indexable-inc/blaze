//! The sockets the daemon and its workers take requests on. One is for its user alone: it is
//! in a directory only the user can enter, only the user can connect to it, and a peer that
//! is another user is turned away. One process serves it: whoever holds the lock next to
//! it. A request is one line, read with a limit on its length and on the time it may take,
//! by a thread of its own, so a client that says nothing holds up no other.

use std::{
    fs::{File, OpenOptions},
    io::{self, BufRead as _, BufReader, Read as _},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
#[cfg(windows)]
use uds_windows::{UnixListener, UnixStream};

#[cfg(unix)]
#[allow(unsafe_code, reason = "who a peer is: system calls std has no wrapper for")]
mod unix;
#[cfg(windows)]
#[allow(unsafe_code, reason = "who a peer and a path are: Win32's security calls")]
pub mod windows;

#[cfg(unix)]
use unix::{peer_user, user};

/// The longest request: a build names every file rustc linked and every source it read.
pub const LONGEST_REQUEST: u64 = 16 << 20;

/// How long a client has to send its request once it is connected.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a client has to take a reply once there is one: a client that asks and never
/// reads does not keep the thread that answers it.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// The most connections served at a time, each by a thread. One more is closed unheard.
pub const MOST_CONNECTIONS: usize = 256;

/// A socket this process alone serves, for as long as this is kept.
pub struct Served {
    listener: UnixListener,
    open: Arc<AtomicUsize>,
    most: usize,
    lock: File,
}

impl Drop for Served {
    fn drop(&mut self) {
        // A process another thread starts has the lock's file open too until it runs its
        // program, and a lock taken with `flock` lasts until every copy is closed or one of them
        // lets go: closing this copy alone could leave the socket taken for a moment.
        let _ = self.lock.unlock();
    }
}

/// One of the connections a socket serves at a time, until it is dropped.
pub struct Open(Arc<AtomicUsize>);

impl Drop for Open {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Why a socket is not served by this process.
#[derive(Debug)]
pub enum NotServed {
    /// Another process holds the lock and serves it.
    Taken,
    Failed(io::Error),
}

impl From<io::Error> for NotServed {
    fn from(error: io::Error) -> NotServed {
        NotServed::Failed(error)
    }
}

impl Served {
    /// Listens on `socket`. The lock next to it is taken first, so of two processes that
    /// start together one listens and the other is told the socket is taken; a socket file
    /// that is there once the lock is held was left by a process that is gone.
    ///
    /// The socket is made in a directory of its own that only the user can enter and is
    /// moved to `socket` once only the user can connect to it: at no time is it somewhere
    /// others reach with the mode the umask gave it, whatever directory `socket` is in.
    pub fn bind(socket: &Path) -> Result<Served, NotServed> {
        #[cfg(unix)]
        SocketAddr::from_pathname(socket).map_err(|error| too_long(socket, error))?;
        let lock = private_file(OpenOptions::new().write(true).create(true), &lock_of(socket))?;
        if lock.try_lock().is_err() {
            return Err(NotServed::Taken);
        }
        let made_in = with_suffix(socket, ".new");
        match std::fs::remove_dir_all(&made_in) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        private_dir(&made_in)?;
        let made = made_in.join("s");
        #[cfg(unix)]
        let listener = match SocketAddr::from_pathname(&made) {
            Ok(address) => UnixListener::bind_addr(&address)?,
            Err(_) => {
                use std::os::unix::fs::{DirBuilderExt as _, symlink};

                // The address fits, but the one it is made at first does not. It is made in
                // the same directory through a short name, so the rename stays atomic.
                let tmp = Path::new("/tmp");
                let nonce: u64 = std::random::random(..);
                let short = tmp.join(format!(".blaze-{nonce:016x}"));
                let through = || -> io::Result<UnixListener> {
                    let target = std::path::absolute(&made_in)?;
                    not_swapped_under(tmp)?;
                    std::fs::DirBuilder::new().mode(0o700).create(&short)?;
                    let alias = short.join("d");
                    let bound =
                        symlink(target, &alias).and_then(|()| UnixListener::bind(alias.join("s")));
                    let _ = std::fs::remove_dir_all(&short);
                    bound
                };
                through().map_err(|error| {
                    let (socket, short) = (socket.display(), short.display());
                    io::Error::new(
                        error.kind(),
                        format!("binding {socket} through {short}: {error}"),
                    )
                })?
            }
        };
        #[cfg(windows)]
        let listener = UnixListener::bind(&made).map_err(|error| too_long(&made, error))?;
        only_the_user_connects(&made)?;
        std::fs::rename(&made, socket)?;
        std::fs::remove_dir(&made_in)?;
        Ok(Served { listener, open: Arc::default(), most: MOST_CONNECTIONS, lock })
    }

    /// The connections made to the socket, one after another, each with its place among
    /// the ones served at a time, or why one could not be taken. One made when there are as
    /// many as that is closed, and said so on the standard error.
    pub fn incoming(&self) -> impl Iterator<Item = io::Result<(UnixStream, Open)>> {
        self.listener.incoming().filter_map(|stream| {
            let stream = match stream {
                Ok(stream) => stream,
                Err(error) => return Some(Err(error)),
            };
            if self.open.fetch_add(1, Ordering::Relaxed) < self.most {
                return Some(Ok((stream, Open(self.open.clone()))));
            }
            self.open.fetch_sub(1, Ordering::Relaxed);
            eprintln!("a connection is closed: {} are open", self.most);
            None
        })
    }
}

/// Connects to the socket at `socket` when the process that serves it is the user's. A
/// socket can be anywhere a command line says, and what is sent to it is the request and
/// the variables of this process: not for another user who listens there first.
pub fn connect(socket: &Path) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(socket).map_err(|error| too_long(socket, error))?;
    check_peer(&stream)?;
    Ok(stream)
}

/// Names the path in an error that it is too long for a socket: one has 104 bytes on
/// macOS and 108 on Linux, with the NUL at its end, and the error alone says neither the
/// path nor how long it is.
fn too_long(path: &Path, error: io::Error) -> io::Error {
    if error.kind() != io::ErrorKind::InvalidInput {
        return error;
    }
    let length = path.as_os_str().len();
    let said = format!(
        "{error}: {} is {length} bytes; BLAZE_HOME can name a shorter directory",
        path.display()
    );
    io::Error::new(error.kind(), said)
}

/// Opens the file at `path` as `options` say, for the user alone: a lock or a log, which
/// has the paths of projects and what their builds print. A link is not followed: in a
/// directory others write to, it could name any file of the user's.
#[cfg(unix)]
pub fn private_file(options: &mut OpenOptions, path: &Path) -> io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let file = options.mode(0o600).custom_flags(libc::O_NOFOLLOW).open(path)?;
    // One that was there keeps the mode it had.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(windows)]
pub fn private_file(options: &mut OpenOptions, path: &Path) -> io::Result<File> {
    options.open(path)
}

fn lock_of(socket: &Path) -> PathBuf {
    with_suffix(socket, ".lock")
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut path = path.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

/// Makes `dir` if it is not there, for the user alone, closes one of the user's that others
/// can enter, and refuses one that is someone else's: whoever can enter it can put a socket
/// of their own in the place of the daemon's.
///
/// The directories above it are checked too, as far as they can be: one that another
/// user owns, or that everyone can write to without the sticky bit, lets someone else put
/// a directory of theirs in the place of this one after it is checked. Root's are trusted,
/// and so is the group of a directory the user owns.
#[cfg(unix)]
pub fn private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
    let made = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir);
    made?;
    let is = std::fs::symlink_metadata(dir)?;
    let refused = |why: &str| {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} {why}", dir.display())))
    };
    if !is.is_dir() {
        return refused("is not a directory");
    }
    if is.uid() != user() {
        return refused("belongs to another user");
    }
    if is.mode() & 0o077 != 0 {
        // The user's own directory, left open by something that made it with the default
        // mode: close it. It was checked not to be a link, and nobody else can swap it.
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let whole = std::path::absolute(dir)?;
    whole.ancestors().skip(1).try_for_each(not_swapped_under)
}

/// Refuses a directory in which another user can rename what the user has made.
#[cfg(unix)]
fn not_swapped_under(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    // A link above is followed: what matters is the directory the name is looked up in.
    let is = std::fs::metadata(dir)?;
    let why = if is.uid() != user() && is.uid() != 0 {
        "belongs to another user"
    } else if is.mode() & 0o002 != 0 && is.mode() & 0o1000 == 0 {
        "can be written to by every user"
    } else {
        return Ok(());
    };
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{} {why}, who can replace what is in it", dir.display()),
    ))
}

/// The same by owner and access control list. A directory made here is closed to whom
/// its parent is open to; one that was there is refused if its list lets others in.
#[cfg(windows)]
pub fn private_dir(dir: &Path) -> io::Result<()> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::create_dir(dir) {
        Ok(()) => windows::only_the_user_opens(dir)?,
        // The user's own directory left open by what made it with the default list, as on
        // unix: closed. A link is not followed, and another user's is refused below.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            if std::fs::symlink_metadata(dir)?.is_dir() && windows::owned_by_user(dir)? {
                windows::only_the_user_opens(dir)?;
            }
        }
        Err(e) => return Err(e),
    }
    if !std::fs::symlink_metadata(dir)?.is_dir() {
        let why = format!("{} is not a directory", dir.display());
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, why));
    }
    windows::the_users_alone(dir)
}

#[cfg(unix)]
fn only_the_user_connects(socket: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
fn only_the_user_connects(socket: &Path) -> io::Result<()> {
    windows::only_the_user_opens(socket)
}

/// Turns away a peer that is another user.
#[cfg(unix)]
fn check_peer(stream: &UnixStream) -> io::Result<()> {
    let peer = peer_user(stream)?;
    if peer == user() {
        return Ok(());
    }
    Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("the peer is user {peer}")))
}

#[cfg(windows)]
use windows::check_peer;

/// Reads the one line a client sends, without its end: of the user, within the time and
/// the length a request may take.
pub fn read_request(stream: &UnixStream) -> io::Result<String> {
    let [request] = read_lines_within(stream, LONGEST_REQUEST, REQUEST_TIMEOUT)?;
    Ok(request)
}

/// What a client of the daemon sends: a frame of [`blaze_protocol`], or a line, an editor's
/// `edit` or a request of a blaze from before the frames.
pub enum Asked {
    Frame(Vec<u8>),
    Line(String),
}

/// Reads the one request a client of the daemon sends, as [`read_request`] does a line. A
/// frame starts with a zero byte, as no line does: its length is less than 16 MiB. The
/// newline after it is for a daemon that reads lines, to answer that it is of another
/// version rather than wait for one.
pub fn read_asked(stream: &UnixStream) -> io::Result<Asked> {
    check_peer(stream)?;
    unless_hung_up(stream.set_write_timeout(Some(REPLY_TIMEOUT)))?;
    unless_hung_up(stream.set_read_timeout(Some(REQUEST_TIMEOUT)))?;
    let mut first = [0];
    io::Read::read_exact(&mut &*stream, &mut first)?;
    let asked = match first[0] {
        0 => {
            let mut framed = io::Read::chain(&first[..], stream.take(LONGEST_REQUEST));
            let item = blaze_protocol::read_frame(&mut framed)?;
            let item = item.ok_or(io::ErrorKind::UnexpectedEof)?;
            let mut end = [0];
            io::Read::read_exact(&mut &*stream, &mut end)?;
            if end != *b"\n" {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "a frame without its end"));
            }
            Asked::Frame(item)
        }
        _ => {
            let rest = BufReader::new(io::Read::chain(&first[..], stream).take(LONGEST_REQUEST));
            Asked::Line(read_line_of(rest)?)
        }
    };
    unless_hung_up(stream.set_read_timeout(None))?;
    Ok(asked)
}

fn read_line_of(mut lines: BufReader<impl io::Read>) -> io::Result<String> {
    let mut line = String::new();
    lines.read_line(&mut line)?;
    match line.strip_suffix('\n') {
        Some(line) => Ok(line.to_owned()),
        None if line.len() as u64 >= LONGEST_REQUEST => {
            Err(io::Error::new(io::ErrorKind::InvalidData, "the request is too long"))
        }
        None => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the request has no end")),
    }
}

/// Reads the two lines a client of a worker sends, as [`read_request`] does one: the
/// request, and the variables of the client's process.
pub fn read_request_and_env(stream: &UnixStream) -> io::Result<[String; 2]> {
    read_lines_within(stream, LONGEST_REQUEST, REQUEST_TIMEOUT)
}

fn read_lines_within<const LINES: usize>(
    stream: &UnixStream,
    longest: u64,
    timeout: Duration,
) -> io::Result<[String; LINES]> {
    check_peer(stream)?;
    unless_hung_up(stream.set_write_timeout(Some(REPLY_TIMEOUT)))?;
    unless_hung_up(stream.set_read_timeout(Some(timeout)))?;
    let mut lines = BufReader::new(stream.take(longest));
    let mut read = [const { String::new() }; LINES];
    for line in &mut read {
        *line = read_line(&mut lines)?;
    }
    // What a build takes is not the client's to be timed for.
    unless_hung_up(stream.set_read_timeout(None))?;
    Ok(read)
}

/// `limited` as it is, but for the refusal macOS gives a time limit on a stream whose peer
/// has hung up. Nothing waits on such a stream, so it needs no limit.
fn unless_hung_up(limited: io::Result<()>) -> io::Result<()> {
    match limited {
        Err(refused) if refused.kind() == io::ErrorKind::InvalidInput => Ok(()),
        limited => limited,
    }
}

/// The next line of `lines` without its end, when it has one before `lines` is at its limit.
fn read_line(lines: &mut BufReader<io::Take<&UnixStream>>) -> io::Result<String> {
    let mut line = String::new();
    lines.read_line(&mut line)?;
    match line.strip_suffix('\n') {
        Some(line) => Ok(line.to_owned()),
        None if lines.get_ref().limit() == 0 => {
            Err(io::Error::new(io::ErrorKind::InvalidData, "the request is too long"))
        }
        None => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the request has no end")),
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn the_directory_and_the_socket_are_the_users_alone() {
        let dir = std::env::temp_dir().join(format!("blaze-socket-private-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        private_dir(&dir).unwrap();
        private_dir(&dir).unwrap();
        let served = Served::bind(&dir.join("sock")).unwrap();
        // The peer of a connection from this process is this user.
        let _client = UnixStream::connect(dir.join("sock")).unwrap();
        let (peer, _) = served.listener.accept().unwrap();
        check_peer(&peer).unwrap();
        windows::the_users_alone(&dir.join("sock")).unwrap();

        let opened = std::process::Command::new("icacls")
            .arg(&dir)
            .args(["/grant", "*S-1-1-0:(OI)(CI)R"])
            .output()
            .unwrap();
        assert!(opened.status.success(), "{opened:?}");
        // The user's own directory that others can enter is closed, not refused.
        private_dir(&dir).unwrap();
        windows::the_users_alone(&dir).unwrap();
        let file = dir.join("file");
        std::fs::write(&file, "").unwrap();
        assert_eq!(private_dir(&file).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        drop(served);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{io::Write as _, os::unix::fs::PermissionsExt as _, time::Instant};

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-socket-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_socket_can_use_the_full_unix_address_length() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let mut name = String::from("s");
        while SocketAddr::from_pathname(dir.path().join(&name)).is_ok() {
            name.push('s');
        }
        let overlong = dir.path().join(&name);
        name.pop();
        let socket = dir.path().join(name);
        let _served = Served::bind(&socket).unwrap();
        assert!(UnixStream::connect(&socket).is_ok());
        assert_eq!(std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(matches!(Served::bind(&overlong), Err(NotServed::Failed(error))
            if error.kind() == io::ErrorKind::InvalidInput));
    }

    #[test]
    fn the_directory_and_the_socket_are_the_users_alone() {
        let dir = scratch("private");
        private_dir(&dir).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        let served = Served::bind(&dir.join("sock")).unwrap();
        assert_eq!(
            std::fs::metadata(dir.join("sock")).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // The peer of a connection from this process is this user.
        let _client = UnixStream::connect(dir.join("sock")).unwrap();
        let (peer, _open) = served.incoming().next().unwrap().unwrap();
        assert_eq!(peer_user(&peer).unwrap(), user());
        check_peer(&peer).unwrap();

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The user's own directory that others can enter is closed, not refused.
        private_dir(&dir).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        let file = dir.join("file");
        std::fs::write(&file, "").unwrap();
        assert_eq!(private_dir(&file).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_silent_or_endless_client_is_given_up_on() {
        let dir = scratch("requests");
        private_dir(&dir).unwrap();
        let served = Served::bind(&dir.join("sock")).unwrap();
        let connect = || {
            let client = UnixStream::connect(dir.join("sock")).unwrap();
            (client, served.incoming().next().unwrap().unwrap().0)
        };
        let short = Duration::from_millis(100);

        let (_silent, stream) = connect();
        let began = Instant::now();
        let timed_out = read_lines_within::<1>(&stream, 64, short).unwrap_err();
        assert!(matches!(timed_out.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut));
        assert!(began.elapsed() < Duration::from_secs(5));

        let (mut endless, stream) = connect();
        endless.write_all(&[b'a'; 200]).unwrap();
        let too_long = read_lines_within::<1>(&stream, 64, short).unwrap_err();
        assert_eq!(too_long.kind(), io::ErrorKind::InvalidData);

        let (mut cut_short, stream) = connect();
        cut_short.write_all(b"sync").unwrap();
        drop(cut_short);
        let no_end = read_lines_within::<1>(&stream, 64, short).unwrap_err();
        assert_eq!(no_end.kind(), io::ErrorKind::UnexpectedEof);

        // The others did not keep this one from being heard.
        let (mut good, stream) = connect();
        good.write_all(b"sync\nA=b\n").unwrap();
        let heard: [String; 2] = read_lines_within(&stream, 64, short).unwrap();
        assert_eq!(heard, ["sync", "A=b"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_socket_in_a_shared_directory_is_never_open_to_others() {
        // What a `--socket` in a directory everyone can enter is given.
        let dir = scratch("shared");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let socket = dir.join("sock");
        let served = Served::bind(&socket).unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&socket), 0o600);
        assert_eq!(mode(&lock_of(&socket)), 0o600);
        // The directory it was made in is gone, and was the user's alone while it was there.
        assert!(!with_suffix(&socket, ".new").exists());
        // The client is as sure of who serves it as the server is of the client.
        let stream = connect(&socket).unwrap();
        assert_eq!(peer_user(&stream).unwrap(), user());
        drop(served);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_log_that_was_there_becomes_the_users_alone() {
        let dir = scratch("log");
        private_dir(&dir).unwrap();
        let log = dir.join("log");
        std::fs::write(&log, "before\n").unwrap();
        std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut file = private_file(OpenOptions::new().create(true).append(true), &log).unwrap();
        file.write_all(b"after\n").unwrap();
        assert_eq!(std::fs::metadata(&log).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "before\nafter\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_directory_others_can_swap_is_refused() {
        let dir = scratch("parents");
        let open = dir.join("open");
        std::fs::create_dir_all(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        let refused = private_dir(&open.join("home")).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        assert!(refused.to_string().contains("written to by every user"), "{refused}");
        // With the sticky bit only the one who made a directory renames it.
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o1777)).unwrap();
        private_dir(&open.join("home")).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_client_that_never_reads_is_given_up_on() {
        let dir = scratch("replies");
        private_dir(&dir).unwrap();
        let served = Served::bind(&dir.join("sock")).unwrap();
        let mut client = connect(&dir.join("sock")).unwrap();
        client.write_all(b"sync\n").unwrap();
        let (stream, _open) = served.incoming().next().unwrap().unwrap();
        read_request(&stream).unwrap();
        stream.set_write_timeout(Some(Duration::from_millis(100))).unwrap();
        let began = Instant::now();
        let reply = [b'r'; 1 << 16];
        let gave_up = loop {
            if let Err(error) = (&stream).write_all(&reply) {
                break error;
            }
            assert!(began.elapsed() < Duration::from_secs(20), "the write never timed out");
        };
        assert!(matches!(gave_up.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_reply_has_a_time_limit_once_the_request_is_read() {
        let dir = scratch("reply-limit");
        private_dir(&dir).unwrap();
        let served = Served::bind(&dir.join("sock")).unwrap();
        let mut client = connect(&dir.join("sock")).unwrap();
        client.write_all(b"sync\n").unwrap();
        let (stream, _open) = served.incoming().next().unwrap().unwrap();
        assert_eq!(stream.write_timeout().unwrap(), None);
        read_request(&stream).unwrap();
        assert_eq!(stream.write_timeout().unwrap(), Some(REPLY_TIMEOUT));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn one_connection_too_many_is_closed() {
        let dir = scratch("most");
        private_dir(&dir).unwrap();
        let mut served = Served::bind(&dir.join("sock")).unwrap();
        served.most = 2;
        let clients: Vec<UnixStream> =
            (0..3).map(|_| connect(&dir.join("sock")).unwrap()).collect();
        let mut incoming = served.incoming();
        let first = incoming.next().unwrap().unwrap();
        let second = incoming.next().unwrap().unwrap();
        assert_eq!(served.open.load(Ordering::Relaxed), 2);
        let fourth = std::thread::scope(|scope| {
            // The third is closed, and the one after it is waited for.
            let next = scope.spawn(|| incoming.next().unwrap().unwrap());
            let mut end = String::new();
            assert_eq!((&clients[2]).read_to_string(&mut end).unwrap_or(0), 0);
            drop(first);
            let _fourth = connect(&dir.join("sock")).unwrap();
            next.join().unwrap()
        });
        assert_eq!(served.open.load(Ordering::Relaxed), 2);
        drop((second, fourth));
        assert_eq!(served.open.load(Ordering::Relaxed), 0);
        drop(incoming);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn one_process_serves_a_socket() {
        let dir = scratch("lock");
        private_dir(&dir).unwrap();
        let socket = dir.join("sock");
        let first = Served::bind(&socket).unwrap();
        // The second leaves the socket of the first where it is.
        assert!(matches!(Served::bind(&socket), Err(NotServed::Taken)));
        assert!(UnixStream::connect(&socket).is_ok());
        drop(first);
        // The file a process left behind is no socket anyone serves, and is replaced.
        assert!(socket.exists());
        let second = Served::bind(&socket).unwrap();
        assert!(UnixStream::connect(&socket).is_ok());
        drop(second);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A socket let go of is free at once, also while another thread starts processes, each
    /// of which has every open file of this one until it runs its program.
    #[test]
    fn a_socket_let_go_of_is_free_while_processes_start() {
        let dir = scratch("lock-forks");
        private_dir(&dir).unwrap();
        let socket = dir.join("sock");
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let starts = std::thread::spawn({
            let done = done.clone();
            move || {
                while !done.load(Ordering::Relaxed) {
                    let _ = std::process::Command::new("true").status();
                }
            }
        });
        // Each bind comes after the one before it was dropped.
        let taken = (0..1000).filter(|_| Served::bind(&socket).is_err()).count();
        done.store(true, Ordering::Relaxed);
        starts.join().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(taken, 0, "the socket was still taken after it was let go of");
    }
}
