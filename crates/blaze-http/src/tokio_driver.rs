//! The tokio driver of [`Client`]: sockets, DNS and timers, nothing else (the `tokio` feature).
//!
//! One task owns the client and loops: it tells the client the time, does the [`Io`] the
//! client asks for (each connect, send and receive a small task of its own, which posts its
//! completion back), hands the client's [`Event`]s to their askers, and sleeps until the next
//! message or the client's next deadline. It keeps the sockets, split into their read and write
//! halves, each half lent to the task using it and given back with the completion: the
//! completion model an io_uring driver has too, so that one is written against the same
//! [`Io`] and [`Completion`] (the crate's driver contract) with nothing here to port.
//!
//! An asker ([`Driver::ask`]) gets an [`Answer`], whose [`Answer::next`] waits for the next
//! event of its request. Taking the next event says the last piece of the body was taken
//! ([`Client::consumed`]), so a slow asker holds the transfer back rather than the driver
//! buffering it; dropping the answer before its end cancels the request ([`Client::cancel`]),
//! closing its connection.
//!
//! The waits here (a connect, a send, a receive, an asker's next event, the loop's sleep) open
//! no span yet: blaze-trace is not in the graph (the crate's documentation). Each is bounded by
//! the client's deadlines, which end every request.

use std::{
    collections::BTreeMap,
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::mpsc,
    task::AbortHandle,
    time::Instant,
};

use crate::client::{Client, Completion, ConnId, Event, Io, Request, RequestId};

/// The most bytes one receive reads.
pub const RECV_MOST: usize = 64 << 10;

/// A handle on a running driver, cheap to clone into any task or thread.
#[derive(Clone)]
pub struct Driver {
    to_loop: mpsc::UnboundedSender<Message>,
    ids: Arc<AtomicU64>,
}

/// What the loop is told: by askers, and by the tasks doing its IO.
enum Message {
    Ask { id: RequestId, request: Request, events: mpsc::UnboundedSender<Event> },
    Consumed { id: RequestId, bytes: usize },
    Cancel { id: RequestId },
    Connected { conn: ConnId, stream: TcpStream },
    ConnectFailed { conn: ConnId, why: String },
    Resolved { conn: ConnId, addresses: Vec<IpAddr> },
    ResolveFailed { conn: ConnId, why: String },
    Sent { conn: ConnId, half: OwnedWriteHalf },
    Received { conn: ConnId, half: OwnedReadHalf, bytes: Vec<u8> },
    Broken { conn: ConnId, why: String },
}

impl Driver {
    /// Starts the loop driving `client` on `runtime`; it runs until the runtime shuts down.
    pub fn spawn(runtime: &tokio::runtime::Handle, client: Client) -> Driver {
        let (to_loop, inbox) = mpsc::unbounded_channel();
        runtime.spawn(run(client, inbox, to_loop.clone()));
        // alloc: the driver's id counter, made once with it.
        Driver { to_loop, ids: Arc::new(AtomicU64::new(0)) }
    }

    /// Starts `request`: its events come from the [`Answer`].
    pub fn ask(&self, request: Request) -> Answer {
        let id = RequestId(self.ids.fetch_add(1, Ordering::Relaxed));
        let (events, from_loop) = mpsc::unbounded_channel();
        // A loop that is gone drops `events`, which the answer reads as the driver's end.
        _ = self.to_loop.send(Message::Ask { id, request, events });
        Answer { id, from_loop, to_loop: self.to_loop.clone(), taken: 0, over: false }
    }
}

/// One request's events, as they come.
pub struct Answer {
    id: RequestId,
    from_loop: mpsc::UnboundedReceiver<Event>,
    to_loop: mpsc::UnboundedSender<Message>,
    /// The bytes of the last piece of the body handed out, said taken at the next ask.
    taken: usize,
    /// Whether its last event (`Done` or `Failed`) came.
    over: bool,
}

impl Answer {
    /// The request's next event; `None` when the driver ended first (its runtime shut down, or
    /// its loop panicked), which no deadline of the client's can then end.
    pub async fn next(&mut self) -> Option<Event> {
        if self.taken > 0 {
            let bytes = std::mem::take(&mut self.taken);
            _ = self.to_loop.send(Message::Consumed { id: self.id, bytes });
        }
        let event = self.from_loop.recv().await?;
        match &event {
            Event::Body { bytes, .. } => self.taken = bytes.len(),
            Event::Done { .. } | Event::Failed { .. } => self.over = true,
            Event::Head { .. } => {}
        }
        Some(event)
    }
}

impl Drop for Answer {
    fn drop(&mut self) {
        if !self.over {
            _ = self.to_loop.send(Message::Cancel { id: self.id });
        }
    }
}

/// A connection's socket: each half here while no task has it.
struct Socket {
    read: Option<OwnedReadHalf>,
    write: Option<OwnedWriteHalf>,
    /// The tasks using its halves, aborted when it closes.
    tasks: Vec<AbortHandle>,
}

/// The loop's state besides the client.
struct Loop {
    to_loop: mpsc::UnboundedSender<Message>,
    sockets: BTreeMap<ConnId, Socket>,
    connecting: BTreeMap<ConnId, AbortHandle>,
    askers: BTreeMap<RequestId, mpsc::UnboundedSender<Event>>,
}

async fn run(
    mut client: Client,
    mut inbox: mpsc::UnboundedReceiver<Message>,
    to_loop: mpsc::UnboundedSender<Message>,
) {
    let start = Instant::now();
    let mut state = Loop {
        to_loop,
        sockets: BTreeMap::new(),
        connecting: BTreeMap::new(),
        askers: BTreeMap::new(),
    };
    loop {
        client.advance_clock(start.elapsed());
        while let Some(io) = client.next_io() {
            state.io(io);
        }
        while let Some(event) = client.next_event() {
            state.event(event);
        }
        let message = match client.next_deadline() {
            Some(due) => match tokio::time::timeout_at(start + due, inbox.recv()).await {
                Ok(message) => message,
                // The deadline: the clock is told at the loop's top.
                Err(_) => continue,
            },
            None => inbox.recv().await,
        };
        // The loop holds a sender itself, so the inbox never ends while it runs.
        let Some(message) = message else { return };
        client.advance_clock(start.elapsed());
        state.message(&mut client, message);
    }
}

impl Loop {
    /// Does `io`, the client's ask.
    fn io(&mut self, io: Io) {
        match io {
            Io::Connect { conn, host, port } => {
                let to_loop = self.to_loop.clone();
                let task = tokio::spawn(async move {
                    let message = match TcpStream::connect((host.as_str(), port)).await {
                        Ok(stream) => Message::Connected { conn, stream },
                        // alloc: an error's message, made only when it fails.
                        Err(e) => Message::ConnectFailed { conn, why: e.to_string() },
                    };
                    _ = to_loop.send(message);
                });
                self.connecting.insert(conn, task.abort_handle());
            }
            Io::Resolve { conn, host } => {
                let to_loop = self.to_loop.clone();
                let task = tokio::spawn(async move {
                    let message = match tokio::net::lookup_host((host.as_str(), 0)).await {
                        Ok(found) => {
                            // alloc: one per name resolved for a proxy, its few addresses.
                            let addresses = found.map(|it| it.ip()).collect();
                            Message::Resolved { conn, addresses }
                        }
                        // alloc: an error's message, made only when it fails.
                        Err(e) => Message::ResolveFailed { conn, why: e.to_string() },
                    };
                    _ = to_loop.send(message);
                });
                // Kept with the connects: a close while it resolves aborts it, and its answer
                // is taken only while the connection is still wanted.
                self.connecting.insert(conn, task.abort_handle());
            }
            Io::Send { conn, bytes } => {
                let Some(socket) = self.sockets.get_mut(&conn) else { return };
                let Some(mut half) = socket.write.take() else { return };
                let to_loop = self.to_loop.clone();
                let task = tokio::spawn(async move {
                    let message = match half.write_all(&bytes).await {
                        Ok(()) => Message::Sent { conn, half },
                        // alloc: an error's message, made only when it fails.
                        Err(e) => Message::Broken { conn, why: e.to_string() },
                    };
                    _ = to_loop.send(message);
                });
                socket.tasks.retain(|it| !it.is_finished());
                socket.tasks.push(task.abort_handle());
            }
            Io::Recv { conn } => {
                let Some(socket) = self.sockets.get_mut(&conn) else { return };
                let Some(mut half) = socket.read.take() else { return };
                let to_loop = self.to_loop.clone();
                let task = tokio::spawn(async move {
                    // alloc: one per receive, handed to the client and dropped after.
                    let mut bytes = Vec::with_capacity(RECV_MOST);
                    let message = match half.read_buf(&mut bytes).await {
                        Ok(_) => Message::Received { conn, half, bytes },
                        // alloc: an error's message, made only when it fails.
                        Err(e) => Message::Broken { conn, why: e.to_string() },
                    };
                    _ = to_loop.send(message);
                });
                socket.tasks.retain(|it| !it.is_finished());
                socket.tasks.push(task.abort_handle());
            }
            Io::Close { conn } => {
                if let Some(task) = self.connecting.remove(&conn) {
                    task.abort();
                }
                // Dropping both halves (the lent ones with their aborted tasks) closes the socket.
                if let Some(socket) = self.sockets.remove(&conn) {
                    for task in socket.tasks {
                        task.abort();
                    }
                }
            }
        }
    }

    /// Hands `event` to its request's asker.
    fn event(&mut self, event: Event) {
        let id = event.request();
        let ends = event.ends();
        if let Some(asker) = self.askers.get(&id) {
            // An asker that is gone has sent its cancel, which the loop reads next.
            _ = asker.send(event);
        }
        if ends {
            self.askers.remove(&id);
        }
    }

    /// Takes `message` into `client`.
    fn message(&mut self, client: &mut Client, message: Message) {
        match message {
            Message::Ask { id, request, events } => {
                if let Err(error) = client.request(id, request) {
                    // The id is another request's: tell this asker, and leave that one be.
                    _ = events.send(Event::Failed { request: id, error });
                    return;
                }
                self.askers.insert(id, events);
            }
            Message::Consumed { id, bytes } => client.consumed(id, bytes),
            Message::Cancel { id } => {
                self.askers.remove(&id);
                client.cancel(id);
            }
            Message::Connected { conn, stream } => {
                // A connection closed while it was being made is dropped here.
                if self.connecting.remove(&conn).is_none() {
                    return;
                }
                let (read, write) = stream.into_split();
                // alloc: one per connection, the handles of its tasks.
                let socket = Socket { read: Some(read), write: Some(write), tasks: Vec::new() };
                self.sockets.insert(conn, socket);
                client.complete(Completion::Connected { conn });
            }
            Message::ConnectFailed { conn, why } => {
                if self.connecting.remove(&conn).is_some() {
                    client.complete(Completion::ConnectFailed { conn, why });
                }
            }
            Message::Resolved { conn, addresses } => {
                if self.connecting.remove(&conn).is_some() {
                    client.complete(Completion::Resolved { conn, addresses });
                }
            }
            Message::ResolveFailed { conn, why } => {
                if self.connecting.remove(&conn).is_some() {
                    client.complete(Completion::ResolveFailed { conn, why });
                }
            }
            Message::Sent { conn, half } => {
                if let Some(socket) = self.sockets.get_mut(&conn) {
                    socket.write = Some(half);
                    client.complete(Completion::Sent { conn });
                }
            }
            Message::Received { conn, half, bytes } => {
                if let Some(socket) = self.sockets.get_mut(&conn) {
                    socket.read = Some(half);
                    client.complete(Completion::Received { conn, bytes });
                }
            }
            Message::Broken { conn, why } => {
                if self.sockets.contains_key(&conn) {
                    client.complete(Completion::Broken { conn, why });
                }
            }
        }
    }
}
