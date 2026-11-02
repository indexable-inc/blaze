//! Warm processes (#804, #805): a proc-macro server or a test binary's fork server started
//! once in its sandbox and kept for the requests that follow, as a state machine that pushes
//! [`Effect`]s and takes [`Completion`]s.
//!
//! A process is found by its [`WarmKey`]: its program's code, its sandbox and the variables
//! blaze sets for it, never a variable the user's shell passes through (`PATH` made a second
//! worker of the same build, #805): what a request reads of those is traced per request. It
//! talks to blaze over a Unix socket the pool listens on, named in [`SOCKET`], since a spawn
//! has no pipes while it runs; the first connection made to it is the process's, and the pool
//! listens for no other. Its sandbox appends what it sees to a live record outside anything the
//! process may write ([`Pool::live`]), read by scope after each answer
//! ([`crate::trace::assemble`]).

use std::{ffi::OsString, fmt::Write as _, path::PathBuf};

use blaze_core::{
    Address, Command, Completion, Conn, Done, Effect, Exit, FileName, IoError, Sandbox, Tag,
};
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher, radix};

use crate::confine::LIVE;

/// The variable that names, to a warm process, the socket to connect to.
pub const SOCKET: &str = "BLAZE_WARM_SOCKET";

/// Which warm process serves a request.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct WarmKey(pub ContentId);

impl WarmKey {
    /// The key of processes of `code` started as `command` is, with the variables `set`
    /// (those blaze sets: the rest of the command's environment is passed through and traced).
    pub fn of(code: &ContentId, command: &Command, set: &[(OsString, OsString)]) -> WarmKey {
        let mut hasher = Hasher::new(crate::trace::os_domain!("blaze v2 warm key 1"));
        hasher.id(code);
        hasher.part(&crate::trace::os_bytes(command.program.as_os_str()));
        hasher.update(&(command.args.len() as u64).to_le_bytes());
        for arg in &command.args {
            hasher.part(&crate::trace::os_bytes(arg));
        }
        hasher.part(&crate::trace::os_bytes(command.dir.as_os_str()));
        blaze_mem::frame::task(blaze_mem::Tag::Usercode, |frame| {
            let mut sorted: Vec<&(OsString, OsString), &blaze_mem::Frame> = Vec::with_capacity_in(set.len(), frame);
            sorted.extend(set);
            sorted.sort_unstable();
            hasher.update(&(sorted.len() as u64).to_le_bytes());
            for (name, value) in sorted.iter() {
                hasher.part(&crate::trace::os_bytes(name)).part(&crate::trace::os_bytes(value));
            }
        });
        match &command.sandbox {
            Sandbox::Off => _ = hasher.update(b"off"),
            Sandbox::On { read, write, network, trace, services } => {
                hasher.update(b"on");
                for list in [read, write] {
                    hasher.update(&(list.len() as u64).to_le_bytes());
                    for path in list {
                        hasher.part(&crate::trace::os_bytes(path.as_os_str()));
                    }
                }
                hasher.update(&[u8::from(*network), u8::from(*trace)]);
                hasher.update(&(services.len() as u64).to_le_bytes());
                for service in services {
                    hasher.part(service.as_bytes());
                }
            }
        }
        hasher.update(&command.memory.unwrap_or(u64::MAX).to_le_bytes());
        WarmKey(hasher.finish())
    }
}

/// One warm process of the pool.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Server(pub u32);

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Config {
    /// The pool's directory under the driver's root: its sockets and live records.
    pub dir: FileName,
    /// The driver's root, as the machine names it: where a process finds its socket.
    pub root: PathBuf,
    /// Tags it asks with are `tags..tags + 2^32`.
    pub tags: u64,
    /// How many idle processes of one key it keeps; one given back past that is stopped.
    pub idle: usize,
}

/// What happened to a process of the pool.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Event {
    /// It connected: it serves requests on `conn` from now on, the caller's until it gives
    /// it back.
    Ready { server: Server, conn: Conn },
    /// It ended, or never started: how, or why not. Whoever had it has it no more.
    Ended { server: Server, exit: Result<Box<Exit>, IoError> },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Started; not connected yet.
    Starting,
    /// Connected and someone's.
    Busy(Conn),
    /// Connected and no one's.
    Idle(Conn),
    /// Asked to stop; its spawn's end is all that is left.
    Stopping,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Asked {
    Spawn(Server),
    Listen(Server),
    Hangup(Server),
}

struct Process {
    key: WarmKey,
    state: State,
    spawn: Tag,
    listen: Option<Tag>,
    /// Why it is of no use, when its pool could not listen for it.
    failed: Option<IoError>,
}

pub struct Pool {
    config: Config,
    next: u64,
    servers: HashMap<Server, Process>,
    asked: HashMap<Tag, Asked>,
    idle: HashMap<WarmKey, Vec<Server>>,
    count: u32,
}

impl Pool {
    pub fn new(config: Config) -> Pool {
        Pool {
            config,
            next: 0,
            servers: HashMap::default(),
            asked: HashMap::default(),
            idle: HashMap::default(),
            count: 0,
        }
    }

    fn tag(&mut self, asked: Asked) -> Tag {
        assert!(self.next < 1 << 32, "the pool has used every tag it was given");
        let tag = Tag(self.config.tags + self.next);
        self.next += 1;
        self.asked.insert(tag, asked);
        tag
    }

    fn file(&self, kind: &str, server: Server) -> FileName {
        blaze_mem::scratch::with_string(|text| {
            let _ = write!(text, "{}/{kind}/{}", self.config.dir, server.0);
            // alloc: the name, which the `FileName` owns.
            FileName::new(text)
        })
        .expect("the pool's directory is a file name, and so is what is under it")
    }

    /// The socket `server` connects to, under the driver's root.
    pub fn socket(&self, server: Server) -> FileName {
        blaze_mem::scratch::with_string(|text| {
            let _ = write!(text, "{}/socket/{}/s", self.config.dir, server.0);
            // alloc: the name, which the `FileName` owns.
            FileName::new(text)
        })
        .expect("a file name")
    }

    /// The live record of `server`'s sandbox, under the driver's root: in no directory the
    /// process may write, so what it says it read is its sandbox's alone.
    pub fn live(&self, server: Server) -> FileName {
        self.file("live", server)
    }

    /// An idle process of `key`, now the caller's, and its connection.
    pub fn take(&mut self, key: WarmKey) -> Option<(Server, Conn)> {
        let server = self.idle.get_mut(&key)?.pop()?;
        let process = self.servers.get_mut(&server).expect("an idle server is a server");
        let State::Idle(conn) = process.state else { unreachable!("an idle server is idle") };
        process.state = State::Busy(conn);
        Some((server, conn))
    }

    /// Starts a process of `key` as `command` says, which must be sandboxed: it is the
    /// caller's once [`Event::Ready`] says it connected. Its socket's directory is the one
    /// thing of the pool's it may write.
    pub fn start(&mut self, key: WarmKey, mut command: Command, effects: &mut Vec<Effect>) -> Server {
        let server = Server(self.count);
        self.count += 1;
        let socket = self.socket(server);
        let socket_path = self.config.root.join(socket.as_str());
        // alloc: the directory the process may write, which its sandbox's command keeps.
        let socket_dir = socket_path.parent().expect("a socket's name is under its directory").to_owned();
        let live = self.config.root.join(self.live(server).as_str());
        command.env.retain(|it| it.0 != SOCKET && it.0 != LIVE);
        command.env.push((SOCKET.into(), socket_path.into_os_string()));
        command.env.push((LIVE.into(), live.into_os_string()));
        if let Sandbox::On { write, .. } = &mut command.sandbox {
            write.push(socket_dir);
        }
        let listen = self.tag(Asked::Listen(server));
        let spawn = self.tag(Asked::Spawn(server));
        effects.push(Effect::Listen { at: Address::Unix(socket), tag: listen });
        // alloc: a spawn, at the edge: the effect owns its command.
        effects.push(Effect::Spawn { command: Box::new(command), tag: spawn });
        self.servers.insert(
            server,
            Process { key, state: State::Starting, spawn, listen: Some(listen), failed: None },
        );
        server
    }

    /// `server`, the caller's, is no one's again: kept for the next request of its key, or
    /// stopped when its key has enough idle ones.
    pub fn give_back(&mut self, server: Server, effects: &mut Vec<Effect>) {
        let Some(process) = self.servers.get_mut(&server) else { return };
        let State::Busy(conn) = process.state else { return };
        let idle = self.idle.entry(process.key).or_default();
        match idle.len() < self.config.idle {
            true => {
                process.state = State::Idle(conn);
                idle.push(server);
            }
            false => self.stop(server, effects),
        }
    }

    /// Stops `server`, whoever's it was: its connection closed and its process killed. Its
    /// [`Event::Ended`] still comes.
    pub fn stop(&mut self, server: Server, effects: &mut Vec<Effect>) {
        let Some(process) = self.servers.get_mut(&server) else { return };
        let conn = match process.state {
            State::Busy(conn) | State::Idle(conn) => Some(conn),
            State::Starting | State::Stopping => None,
        };
        process.state = State::Stopping;
        let (spawn, listen, key) = (process.spawn, process.listen.take(), process.key);
        if let Some(idle) = self.idle.get_mut(&key) {
            idle.retain(|it| *it != server);
        }
        if let Some(listen) = listen {
            effects.push(Effect::Unlisten { tag: listen });
        }
        if let Some(conn) = conn {
            let tag = self.tag(Asked::Hangup(server));
            effects.push(Effect::Hangup { conn, tag });
        }
        effects.push(Effect::Kill { tag: spawn });
    }

    /// Stops every process: the daemon is ending.
    pub fn stop_all(&mut self, effects: &mut Vec<Effect>) {
        // alloc: once, as the daemon ends.
        let mut servers: Vec<Server> = self.servers.keys().copied().collect();
        radix::sort_copy_by_key(&mut servers, |it| it.0);
        for server in servers {
            self.stop(server, effects);
        }
    }

    pub fn owns(&self, tag: Tag) -> bool {
        self.asked.contains_key(&tag)
    }

    /// How many processes it has, starting, busy, idle or stopping.
    pub fn len(&self) -> usize {
        self.servers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// Takes a completion of its own.
    pub fn complete(&mut self, completion: Completion, effects: &mut Vec<Effect>) -> Option<Event> {
        let asked = *self.asked.get(&completion.tag)?;
        match (asked, completion.done) {
            (Asked::Listen(server), Done::Accepted { conn }) => {
                let process = self.servers.get_mut(&server)?;
                match process.state {
                    State::Starting => {
                        process.state = State::Busy(conn);
                        // One connection per process: the next one made to its socket is
                        // not its.
                        if let Some(listen) = process.listen.take() {
                            effects.push(Effect::Unlisten { tag: listen });
                        }
                        Some(Event::Ready { server, conn })
                    }
                    _ => {
                        let tag = self.tag(Asked::Hangup(server));
                        effects.push(Effect::Hangup { conn, tag });
                        None
                    }
                }
            }
            (Asked::Listen(_), Done::Unlistened) => {
                self.asked.remove(&completion.tag);
                None
            }
            (Asked::Listen(server), Done::Failed(error)) => {
                self.asked.remove(&completion.tag);
                // It can't be reached: it is of no use, and its end says why.
                let socket = self.socket(server);
                if let Some(process) = self.servers.get_mut(&server) {
                    process.listen = None;
                    // alloc: an error, made once as the process fails.
                    process.failed = Some(IoError(format!(
                        "its socket {socket} could not be listened on: {error}"
                    )));
                }
                self.stop(server, effects);
                None
            }
            (Asked::Spawn(server), done) => {
                self.asked.remove(&completion.tag);
                let process = self.servers.remove(&server)?;
                if let Some(idle) = self.idle.get_mut(&process.key) {
                    idle.retain(|it| *it != server);
                }
                if let Some(listen) = process.listen {
                    effects.push(Effect::Unlisten { tag: listen });
                }
                if let State::Busy(conn) | State::Idle(conn) = process.state {
                    let tag = self.tag(Asked::Hangup(server));
                    effects.push(Effect::Hangup { conn, tag });
                }
                let exit = match (process.failed, done) {
                    (Some(failed), _) => Err(failed),
                    (None, Done::Exited(exit)) => Ok(exit),
                    (None, Done::Failed(error)) => Err(error),
                    // alloc: an error, made once as the spawn ends.
                    (None, other) => Err(IoError(format!("a spawn completed as {other:?}"))),
                };
                Some(Event::Ended { server, exit })
            }
            (Asked::Hangup(_), _) => {
                self.asked.remove(&completion.tag);
                None
            }
            (Asked::Listen(_), _) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config { dir: FileName::new("pool").unwrap(), root: "/root".into(), tags: 1 << 40, idle: 1 }
    }

    fn command(path: &str) -> Command {
        Command {
            program: "/srv".into(),
            args: Vec::new(),
            env: vec![("PATH".into(), path.into()), ("CARGO".into(), "c".into())],
            dir: "/ws".into(),
            stdin: Vec::new(),
            sandbox: Sandbox::On { read: Vec::new(), write: Vec::new(), network: false, trace: true, services: Vec::new() },
            memory: None,
        }
    }

    fn tags(effects: &[Effect]) -> (Tag, Tag) {
        let listen = effects.iter().find_map(|it| matches!(it, Effect::Listen { .. }).then(|| it.tag()));
        let spawn = effects.iter().find_map(|it| matches!(it, Effect::Spawn { .. }).then(|| it.tag()));
        (listen.unwrap(), spawn.unwrap())
    }

    #[test]
    fn a_path_passed_through_is_not_a_new_server() {
        let set = [("CARGO".into(), "c".into())];
        let code = ContentId::of("blaze v2 test 1", b"srv");
        assert_eq!(WarmKey::of(&code, &command("/a"), &set), WarmKey::of(&code, &command("/b"), &set));
        let other = [("CARGO".into(), "d".into())];
        assert_ne!(WarmKey::of(&code, &command("/a"), &set), WarmKey::of(&code, &command("/a"), &other));
    }

    /// A process allowed other Mach lookups is another process.
    #[test]
    fn the_services_a_server_may_look_up_are_in_its_key() {
        let set = [("CARGO".into(), "c".into())];
        let code = ContentId::of("blaze v2 test 1", b"srv");
        let mut named = command("/a");
        let Sandbox::On { services, .. } = &mut named.sandbox else { panic!() };
        services.push("dev.blaze.inproc.1".into());
        assert_ne!(WarmKey::of(&code, &command("/a"), &set), WarmKey::of(&code, &named, &set));
        let mut other = command("/a");
        let Sandbox::On { services, .. } = &mut other.sandbox else { panic!() };
        services.push("dev.blaze.inproc.2".into());
        assert_ne!(WarmKey::of(&code, &named, &set), WarmKey::of(&code, &other, &set));
    }

    #[test]
    fn a_server_starts_connects_is_kept_and_ends() {
        let mut pool = Pool::new(config());
        let key = WarmKey(ContentId::of("blaze v2 test 1", b"k"));
        let mut effects = Vec::new();
        let server = pool.start(key, command("/a"), &mut effects);
        let (listen, spawn) = tags(&effects);
        let Effect::Spawn { command, .. } = &effects[1] else { panic!() };
        assert!(command.env.iter().any(|it| it.0 == SOCKET && it.1 == "/root/pool/socket/0/s"));
        assert!(command.env.iter().any(|it| it.0 == LIVE && it.1 == "/root/pool/live/0"));
        let Sandbox::On { write, .. } = &command.sandbox else { panic!() };
        assert_eq!(write, &[PathBuf::from("/root/pool/socket/0")]);
        assert!(pool.owns(listen) && pool.owns(spawn));
        effects.clear();
        assert_eq!(pool.take(key), None);
        let ready = pool.complete(Completion { tag: listen, done: Done::Accepted { conn: Conn(5) } }, &mut effects);
        assert_eq!(ready, Some(Event::Ready { server, conn: Conn(5) }));
        assert_eq!(effects, vec![Effect::Unlisten { tag: listen }]);
        effects.clear();
        pool.give_back(server, &mut effects);
        assert!(effects.is_empty());
        assert_eq!(pool.take(key), Some((server, Conn(5))));
        let exit = Exit { code: Some(0), signal: None, stdout: Vec::new(), stderr: Vec::new(), traced: None, peak_memory: 0 };
        let ended = pool.complete(Completion { tag: spawn, done: Done::Exited(Box::new(exit.clone())) }, &mut effects);
        assert_eq!(ended, Some(Event::Ended { server, exit: Ok(Box::new(exit)) }));
        assert!(effects.iter().any(|it| matches!(it, Effect::Hangup { conn: Conn(5), .. })));
        assert!(pool.is_empty());
        assert_eq!(pool.take(key), None);
    }

    #[test]
    fn past_the_idle_limit_a_server_given_back_is_stopped() {
        let mut pool = Pool::new(config());
        let key = WarmKey(ContentId::of("blaze v2 test 1", b"k"));
        let mut effects = Vec::new();
        let one = pool.start(key, command("/a"), &mut effects);
        let (listen_one, _) = tags(&effects);
        effects.clear();
        let two = pool.start(key, command("/a"), &mut effects);
        let (listen_two, spawn_two) = tags(&effects);
        effects.clear();
        pool.complete(Completion { tag: listen_one, done: Done::Accepted { conn: Conn(1) } }, &mut effects);
        pool.complete(Completion { tag: listen_two, done: Done::Accepted { conn: Conn(2) } }, &mut effects);
        effects.clear();
        pool.give_back(one, &mut effects);
        pool.give_back(two, &mut effects);
        assert!(effects.contains(&Effect::Kill { tag: spawn_two }));
        assert_eq!(pool.take(key), Some((one, Conn(1))));
    }

    /// A socket's name is formatted in the thread's string: once that has grown, the
    /// `FileName` it makes is all it allocates. A warm key is hashed in the task's frame.
    #[test]
    fn a_socket_name_allocates_only_itself_and_a_warm_key_nothing() {
        let pool = Pool::new(config());
        assert_eq!(pool.socket(Server(7)).as_str(), "pool/socket/7/s");
        let name = blaze_alloc_count::assert_at_most(1, "a socket's name: the FileName alone", || pool.socket(Server(7)));
        assert_eq!(name.as_str(), "pool/socket/7/s");
        let (code, command) = (ContentId::of("blaze v2 test 1", b"srv"), command("/a"));
        let set = [("CARGO".into(), "c".into()), ("A".into(), "1".into())];
        let cold = WarmKey::of(&code, &command, &set);
        let warm = blaze_alloc_count::assert_at_most(0, "a warm key", || WarmKey::of(&code, &command, &set));
        assert_eq!(warm, cold);
    }
}
