//! The requests of a [`Uring`] through the ring, over the columns of [`super::ops`]: each
//! request asked of the kernel as one SQE or a linked chain ([`Uring::sqes`]), its completions
//! stepped in place by its handle ([`Uring::completed`]), and what it held let go as it ends
//! ([`Uring::end`]). Nothing is moved out of a slot while its completion is stepped: a step
//! reads the 32-byte hot record, and the buffer or the cold record only where its kind needs
//! them. A request's [`Hot::links`] has a bit for each link asked whose last completion is not
//! in, which is what the kernel still owes it. Requests on paths are `super::files`'.

use std::{
    mem,
    os::fd::{AsRawFd as _, OwnedFd, RawFd},
};

use super::{
    ops::{ABANDONED, DIRECT, FINISHED, LINKS, NO_BUFFER, STDERR, STOPPING, ZERO_COPY},
    *,
};

/// Why a request failed, as its completion says it: called only for one that did.
pub(super) fn error(res: i32) -> String {
    // alloc: an error path.
    io::Error::from_raw_os_error(-res).to_string()
}

pub(super) fn failed(res: i32) -> Option<Done> {
    Some(Done::Failed(IoError(error(res))))
}

/// The descriptor of `fd`, or -1 for none.
pub(super) fn raw(fd: &Option<OwnedFd>) -> RawFd {
    fd.as_ref().map_or(-1, |it| it.as_raw_fd())
}

/// Where a read into `bytes` puts what it reads, and how much it may: the spare capacity after
/// its length, which is never zeroed first (the kernel's copy is the one pass over the bytes).
pub(super) fn spare_of(bytes: &[u8], capacity: usize) -> (u64, u32) {
    let spare = capacity - bytes.len();
    (bytes.as_ptr().wrapping_add(bytes.len()) as u64, spare.min(u32::MAX as usize) as u32)
}

/// `res` bytes the kernel read into `bytes`' spare capacity ([`spare_of`]) counted in its
/// length; whether there is room for more.
pub(super) fn filled(bytes: &mut Vec<u8>, res: i32) -> bool {
    let spare = bytes.capacity() - bytes.len();
    // The kernel's count, clamped to the room it was given, so a wrong one never counts bytes
    // past the allocation.
    let got = usize::try_from(res).unwrap_or(0).min(spare);
    // SAFETY: the read was given the `spare` bytes after `bytes.len()` (`spare_of`) and its
    // completion says the kernel wrote the first `res` of them; `got` is at most that and at
    // most `spare`, so every byte now in the length is initialized and inside the allocation.
    unsafe { bytes.set_len(bytes.len() + got) };
    bytes.len() < bytes.capacity()
}

/// The completions the kernel still owes the request `hot`: one per link asked whose last
/// completion is not in (so an open ahead of a chain is one more), and for a zero-copy send
/// one per notification owed and one for its send in flight.
fn owed(hot: &Hot) -> usize {
    match hot.kind {
        Kind::Send if hot.flags & ZERO_COPY != 0 => {
            usize::from(hot.notifs) + usize::from(hot.flags & FINISHED == 0)
        }
        _ => hot.links.count_ones() as usize,
    }
}

/// Whether the request of `kind` is one on a path, or a close of a slot (`super::files`).
fn on_path(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::WriteAt
            | Kind::Write
            | Kind::Rename
            | Kind::Open
            | Kind::Close
            | Kind::Remove
            | Kind::Link
            | Kind::Clone
            | Kind::Map
            | Kind::Stat
            | Kind::Load
    )
}

impl Uring {
    /// Where the listen `listen` listens, while it does: the port a TCP listen of port 0 was
    /// given. `None` for a Unix socket's.
    pub fn local_addr(&self, listen: Tag) -> Option<std::net::SocketAddr> {
        let id = self.listens.get(&listen)?;
        self.listeners.get(Handle::from_bits(self.ops.get(*id)?.of))?.local_addr()
    }

    /// The request `hot`, lending the kernel `buf` and holding `cold`, asked of the kernel, or
    /// kept until there is room.
    pub(super) fn start(&mut self, hot: Hot, buf: Vec<u8>, cold: Cold) -> Id {
        let id = self.ops.insert(hot, buf, cold);
        self.queue(id);
        id
    }

    /// Stops the request `target`; the cancel's own completion is nobody's.
    fn cancel(&mut self, target: Id) {
        self.start(Hot::new(NOBODYS, Kind::Cancel, data(target, 0)), ops::NO_BYTES, Cold::EMPTY);
    }

    /// Writes the request `id` as it now stands to the ring, or keeps it for when there is
    /// room (or for its kept file's open, the slots its opens fill, or, bypassing the page
    /// cache, a registered buffer).
    pub(super) fn queue(&mut self, id: Id) {
        if !self.admitted(id) || !self.prepared(id) || !self.slotted(id) || !self.buffered(id) {
            return;
        }
        let mut sqes = [Sqe::default(); LINKS];
        let n = self.sqes(id, &mut sqes);
        if !self.waiting.is_empty() || !self.ring.push(&sqes[..n]) {
            self.waiting.push_back(id);
            return;
        }
        self.asked(id, &sqes[..n]);
    }

    /// The links of `sqes`, written to the ring for the request `id`, owed by the kernel.
    fn asked(&mut self, id: Id, sqes: &[Sqe]) {
        let links = sqes.iter().fold(0u8, |links, it| links | 1 << (it.user_data & LINK_MASK));
        self.ops.get_mut(id).expect("a request in flight").links |= links;
    }

    /// Whether the request `id` has the registered buffer it needs, given one if it needs one
    /// and one is free; one that finds none waits in [`ops::Lent`] for the next given back.
    fn buffered(&mut self, id: Id) -> bool {
        let Uring { ops, lent, .. } = self;
        let hot = ops.get_mut(id).expect("a request in flight");
        if hot.kind != Kind::WriteAt || hot.flags & DIRECT == 0 || hot.buffer != NO_BUFFER {
            return true;
        }
        match lent.take(id) {
            Some(buffer) => {
                hot.buffer = buffer;
                true
            }
            None => false,
        }
    }

    /// The registered buffer of a write that ended free again, for the write that has waited
    /// longest.
    pub(super) fn unbuffer(&mut self, buffer: u16) {
        let Uring { ops, lent, .. } = self;
        let Some((id, buffer)) = lent.give(buffer, |it| ops.contains(it)) else { return };
        ops.get_mut(id).expect("in flight, as `give` checked").buffer = buffer;
        self.queue(id);
    }

    /// The request `id` taken out of its slot, and what it held let go: the file it used, its
    /// names, its buffer kept for the next, a spawn's part ended, a connection's use.
    pub(super) fn end(&mut self, id: Id) {
        let (hot, buf, cold) = self.ops.remove(id).expect("a request in flight");
        match hot.kind {
            Kind::Read | Kind::Append | Kind::Sync | Kind::Truncate => {
                unkeep(&mut self.kept, Handle::from_bits(hot.of));
            }
            Kind::Close => {}
            kind if on_path(kind) => self.path_end(&hot, cold),
            Kind::Watch => {
                let Watching { tree, buffer, mut flushes, .. } =
                    self.watchings.remove(Handle::from_bits(hot.of)).expect("a watch's read's watching");
                self.give_back(buffer);
                let spare = tree.end();
                if self.trees.len() < SPARES {
                    flushes.clear();
                    self.trees.push((spare, flushes));
                }
            }
            Kind::Exit => self.part_ended(Handle::from_bits(hot.of)),
            Kind::Output => {
                // Closes the pipe before the process is reaped.
                drop(cold);
                self.give_back(buf);
                self.part_ended(Handle::from_bits(hot.of));
            }
            Kind::Input => {
                drop(cold);
                self.part_ended(Handle::from_bits(hot.of));
            }
            Kind::Recv => {
                self.release(Conn(hot.of));
                self.deliver(Conn(hot.of));
            }
            Kind::Send | Kind::Shutdown => self.release(Conn(hot.of)),
            Kind::Accept => _ = self.listeners.remove(Handle::from_bits(hot.of)),
            Kind::Connect => _ = self.connectings.remove(Handle::from_bits(hot.of)),
            _ => {}
        }
    }

    /// The SQEs the request `id` is, as it now stands, into `sqes`: one, or each link of a
    /// chain. How many.
    fn sqes(&self, id: Id, sqes: &mut [Sqe; LINKS]) -> usize {
        let hot = self.ops.get(id).expect("a request in flight");
        if on_path(hot.kind) {
            return self.path_sqes(id, sqes);
        }
        let (cold, buf) = (self.ops.cold(id), self.ops.buf(id));
        let user_data = data(id, 0);
        let done = hot.done as usize;
        // What of its buffer is still to go: where it starts and how long it is.
        let rest = || (buf[done..].as_ptr() as u64, (buf.len() - done).min(u32::MAX as usize) as u32);
        let kept = || self.kept.get(Handle::from_bits(hot.of)).expect("held by its request").fixed.target();
        let socket = || self.conns.get(conn_of(Conn(hot.of))).expect("used by its request").socket.target();
        // Every buffer and address below stays where it is until the request's last
        // completion: the columns and the slabs are reserved memory that never moves, and
        // what a request holds in them stays until it ends (`Uring::end`).
        sqes[0] = match hot.kind {
            Kind::Read => {
                let ((fd, flags), (addr, len)) = (kept(), spare_of(buf, buf.capacity()));
                let off = cold.at + buf.len() as u64;
                Sqe { opcode: OP_READ, flags, fd, off, addr, len, user_data, ..Sqe::default() }
            }
            Kind::Append => {
                let ((fd, flags), (addr, len)) = (kept(), rest());
                // The file's position, which is its end: it is open for appending.
                Sqe { opcode: OP_WRITE, flags, fd, off: u64::MAX, addr, len, user_data, ..Sqe::default() }
            }
            Kind::Sync => {
                let (fd, flags) = kept();
                Sqe { opcode: OP_FSYNC, flags, fd, user_data, ..Sqe::default() }
            }
            Kind::Watch => {
                let watching = self.watchings.get(Handle::from_bits(hot.of)).expect("a watch's read's watching");
                Sqe {
                    opcode: OP_READ,
                    fd: watching.tree.raw(),
                    off: u64::MAX,
                    addr: watching.buffer.as_ptr() as u64,
                    len: watching.buffer.len() as u32,
                    user_data,
                    ..Sqe::default()
                }
            }
            Kind::Cancel => Sqe { opcode: OP_ASYNC_CANCEL, fd: -1, addr: hot.of, user_data, ..Sqe::default() },
            Kind::Exit => {
                let spawn = self.spawns.get(Handle::from_bits(hot.of)).expect("an exit's spawn, in flight while it is");
                Sqe { opcode: OP_POLL_ADD, fd: spawn.pidfd.as_raw_fd(), op_flags: libc::POLLIN as u32, user_data, ..Sqe::default() }
            }
            // A pipe has no position.
            Kind::Output => Sqe {
                opcode: OP_READ,
                fd: raw(&cold.fds[0]),
                off: u64::MAX,
                addr: buf.as_ptr() as u64,
                len: buf.len() as u32,
                user_data,
                ..Sqe::default()
            },
            Kind::Input => {
                let (addr, len) = rest();
                Sqe { opcode: OP_WRITE, fd: raw(&cold.fds[0]), off: u64::MAX, addr, len, user_data, ..Sqe::default() }
            }
            Kind::Accept => Sqe {
                opcode: OP_ACCEPT,
                ioprio: ACCEPT_MULTISHOT,
                fd: self.listeners.get(Handle::from_bits(hot.of)).expect("an accept's listener").fd(),
                op_flags: libc::SOCK_CLOEXEC as u32,
                // Into a slot of the kernel's half of the table, which the completion names.
                splice_fd_in: FILE_INDEX_ALLOC as i32,
                user_data,
                ..Sqe::default()
            },
            Kind::Connect => {
                let connecting = self.connectings.get(Handle::from_bits(hot.of)).expect("a connect's connecting");
                let (fd, flags) = connecting.socket.as_ref().expect("a connect's socket").target();
                Sqe {
                    opcode: OP_CONNECT,
                    flags,
                    fd,
                    addr: &raw const connecting.at.storage as u64,
                    // The address's length.
                    off: u64::from(connecting.at.len),
                    user_data,
                    ..Sqe::default()
                }
            }
            Kind::Recv => {
                let (fd, flags) = socket();
                Sqe {
                    opcode: OP_RECV,
                    flags: flags | SQE_BUFFER_SELECT,
                    ioprio: RECV_MULTISHOT,
                    fd,
                    buf_index: RECV_GROUP,
                    user_data,
                    ..Sqe::default()
                }
            }
            Kind::Send => {
                let ((fd, flags), (addr, len)) = (socket(), rest());
                let opcode = if hot.flags & ZERO_COPY != 0 { OP_SEND_ZC } else { OP_SEND };
                Sqe { opcode, flags, fd, addr, len, op_flags: libc::MSG_NOSIGNAL as u32, user_data, ..Sqe::default() }
            }
            Kind::Shutdown => {
                let (fd, flags) = socket();
                Sqe { opcode: OP_SHUTDOWN, flags, fd, len: libc::SHUT_RDWR as u32, user_data, ..Sqe::default() }
            }
            Kind::Wake => Sqe {
                opcode: OP_READ,
                fd: self.offload.wake.as_raw_fd(),
                off: u64::MAX,
                addr: self.offload.count.get() as u64,
                len: 8,
                user_data,
                ..Sqe::default()
            },
            kind => unreachable!("{kind:?} is never asked of the kernel this way"),
        };
        1
    }

    /// Work for the effect `tag` was handed off the ring: the ring reads its wake until every
    /// such work's completion is in.
    pub(super) fn offloaded(&mut self, tag: Tag) {
        self.offload.out += 1;
        if self.offload.reading.is_none() {
            let id = self.start(Hot::new(tag, Kind::Wake, 0), ops::NO_BYTES, Cold::EMPTY);
            self.offload.reading = Some(id);
        }
    }

    /// The slots let go emptied, then the requests kept for room written to the ring, in order.
    pub(super) fn flush_waiting(&mut self) {
        self.drain_closing();
        let mut sqes = [Sqe::default(); LINKS];
        while let Some(&id) = self.waiting.front() {
            if !self.ops.contains(id) {
                self.waiting.pop_front();
                continue;
            }
            let n = self.sqes(id, &mut sqes);
            if !self.ring.push(&sqes[..n]) {
                break;
            }
            self.waiting.pop_front();
            self.asked(id, &sqes[..n]);
        }
    }

    /// Shows the kernel what was written to the ring, waking its thread where it sleeps.
    pub(super) fn submit_ring(&mut self) {
        if self.ring.unsubmitted > 0
            && let Err(error) = self.ring.enter(None)
        {
            self.fail_unsubmitted(error);
        }
    }

    /// Every completion posted, stepped in one pass over the ring, then the buffers given back
    /// and the requests written meanwhile shown to the kernel.
    pub(super) fn reap(&mut self) {
        self.take_offloaded();
        let (first, tail) = self.ring.completions();
        let mut head = first;
        while head != tail {
            let (user_data, res, flags) = self.ring.cqe(head);
            head = head.wrapping_add(1);
            self.completed(user_data, res, flags);
        }
        if head != first {
            self.ring.consume(head);
        }
        self.provided.publish();
        self.flush_waiting();
        self.submit_ring();
    }

    /// The ring takes no more requests: every effect in flight fails. What the kernel may
    /// still hold of them stays in its slot, abandoned, never freed, and so do the watches,
    /// spawns' pipes and connections their buffers and files are.
    pub(super) fn fail_unsubmitted(&mut self, error: io::Error) {
        // alloc: clone: the error path, once, when the ring dies.
        let why = format!("the ring failed: {error}");
        // clone: the error path, once per effect in flight.
        let failed = |tag| Completion { tag, done: Done::Failed(IoError(why.clone())) };
        let Uring { ops, watchings, ready, .. } = self;
        for (_, hot) in ops.iter_mut().filter(|(_, it)| it.flags & ABANDONED == 0) {
            hot.flags |= ABANDONED;
            match hot.kind {
                Kind::Watch => {
                    let watching = watchings.get_mut(Handle::from_bits(hot.of)).expect("a watch's read's watching");
                    ready.extend(watching.flushes.drain(..).map(failed));
                    ready.push(failed(hot.tag));
                }
                // A spawn fails once, below; work off the ring still completes through its
                // thread; a receive's effects fail with their connection's, below; a kept
                // file's open, and a close, are no effect's, and the requests waiting for an
                // open fail here as their own.
                Kind::Cancel
                | Kind::Exit
                | Kind::Output
                | Kind::Input
                | Kind::Recv
                | Kind::Wake
                | Kind::Open
                | Kind::Close => {}
                // The kernel may still read its bytes: they stay in the slot, abandoned, and an
                // empty buffer comes back in their place.
                Kind::Send => {
                    let done = Done::unsent(Vec::new(), &why); // alloc: none, an empty `Vec`.
                    ready.push(Completion { tag: hot.tag, done });
                }
                _ => ready.push(failed(hot.tag)),
            }
        }
        // Timers need no ring, but end with it as everything in flight does.
        self.timers.drain(|_, tag| self.ready.push(failed(tag)));
        // alloc: the error path, once.
        let mut spawns: Vec<(u64, Handle<Spawning>)> = self.spawning.drain().map(|(tag, it)| (tag.0, it)).collect();
        radix::sort_copy_by_key(&mut spawns, |it| it.0);
        for (_, spawn) in spawns {
            let mut spawn = self.spawns.remove(spawn).expect("in flight");
            if !spawn.exited {
                _ = kill(spawn.pidfd.as_raw_fd());
            }
            spawn.failed = Some(why.clone()); // clone: the error path.
            let tag = spawn.tag;
            self.ready.push(Completion { tag, done: spawn.end() });
        }
        for (_, conn) in self.conns.iter_mut() {
            for (tag, _, into) in conn.recvs.drain(..) {
                self.ready.push(Completion { tag, done: Done::unread(into, &why) });
            }
            conn.receiving = None;
        }
        self.watches.clear();
        self.listens.clear();
        self.offload.reading = None;
        self.waiting.clear();
        self.on_open.clear();
        self.slot_waiting.clear();
        self.load_waiting.clear();
        drop(self.lent.clear_waiting());
        self.ring.unsubmitted = 0;
    }

    /// Takes the completion whose `user_data` is `user_data`: its effect's completion, or the
    /// rest of it asked for. With [`CQE_F_MORE`] in `flags` the kernel still holds the request
    /// (a multishot one), so it is kept and not asked again.
    ///
    /// A completion whose handle is stale (its request ended, its slot empty or another's since:
    /// a read cancelled after it completed, a request let go when the ring failed) is dropped,
    /// as is one of a request abandoned in place.
    pub(super) fn completed(&mut self, user_data: u64, res: i32, flags: u32) {
        let (id, link) = of_data(user_data);
        let Some(hot) = self.ops.get_mut(id) else { return };
        if hot.flags & ABANDONED != 0 {
            return;
        }
        if flags & CQE_F_MORE == 0 {
            hot.links &= !(1 << link);
        }
        let (tag, kind) = (hot.tag, hot.kind);
        // A zero-copy send's result carries `CQE_F_MORE` for the notification that follows: it
        // is not a multishot request, and its step says when it is asked again.
        let more = flags & CQE_F_MORE != 0 && kind != Kind::Send;
        match self.step(id, link, res, flags) {
            Step::Again => {
                if !more {
                    self.queue(id);
                }
            }
            Step::Goes(done) => {
                self.ready.push(Completion { tag, done });
                if !more {
                    self.queue(id);
                }
            }
            Step::Held => {}
            Step::Ends(done) => {
                if let Some(done) = done {
                    self.ready.push(Completion { tag, done });
                }
                self.end(id);
            }
        }
    }

    /// `res` more bytes of the request `id` done; how many are, in all.
    pub(super) fn progress(&mut self, id: Id, res: i32) -> usize {
        let hot = self.ops.get_mut(id).expect("a request in flight");
        hot.done += res as u64;
        hot.done as usize
    }

    /// What the result `res` of link `link` of the request `id` means for it.
    fn step(&mut self, id: Id, link: usize, res: i32, flags: u32) -> Step {
        let hot = *self.ops.get(id).expect("a request in flight");
        match hot.kind {
            Kind::Cancel => Step::Ends(None),
            Kind::Read => {
                if res < 0 {
                    return Step::Ends(failed(res));
                }
                let bytes = self.ops.buf_mut(id);
                if res > 0 && filled(bytes, res) {
                    return Step::Again;
                }
                Step::Ends(Some(Done::Read(mem::take(bytes))))
            }
            Kind::Append => {
                if res < 0 {
                    return Step::Ends(failed(res));
                }
                let put = self.progress(id, res);
                match (put < self.ops.buf(id).len(), res) {
                    (false, _) => Step::Ends(Some(Done::Appended)),
                    (true, 0) => Step::Ends(failed(-libc::EIO)),
                    (true, _) => Step::Again,
                }
            }
            Kind::Sync if res < 0 => Step::Ends(failed(res)),
            Kind::Sync => Step::Ends(Some(Done::Synced)),
            Kind::WriteAt
            | Kind::Write
            | Kind::Rename
            | Kind::Open
            | Kind::Close
            | Kind::Remove
            | Kind::Link
            | Kind::Clone
            | Kind::Map
            | Kind::Stat
            | Kind::Load => self.path_step(id, link, res),
            Kind::Truncate => unreachable!("a truncate is done as it is prepared"),
            Kind::Watch => self.watched(hot, res),
            Kind::Exit => {
                let spawn = Handle::from_bits(hot.of);
                if res < 0 {
                    self.spoil(spawn, format!("waiting for its exit: {}", error(res))); // alloc: an error.
                } else if let Some(spawn) = self.spawns.get_mut(spawn) {
                    spawn.exited = true;
                }
                Step::Ends(None)
            }
            Kind::Output => {
                let spawn = Handle::from_bits(hot.of);
                let stderr = hot.flags & STDERR != 0;
                if res > 0 {
                    if let Some(it) = self.spawns.get_mut(spawn) {
                        let to = if stderr { &mut it.stderr } else { &mut it.stdout };
                        // alloc: the spawn's own output, grown as it comes.
                        to.extend_from_slice(&self.ops.buf(id)[..res as usize]);
                    }
                    return Step::Again;
                }
                if res < 0 {
                    let pipe = if stderr { "stderr" } else { "stdout" };
                    self.spoil(spawn, format!("reading its {pipe}: {}", error(res))); // alloc: an error.
                }
                Step::Ends(None)
            }
            Kind::Input => {
                let spawn = Handle::from_bits(hot.of);
                // It ended, or closed its input, before reading all of it.
                if res == -libc::EPIPE {
                    return Step::Ends(None);
                }
                let len = self.ops.buf(id).len();
                if res < 0 || (res == 0 && (hot.done as usize) < len) {
                    let why = if res < 0 { error(res) } else { error(-libc::EIO) };
                    self.spoil(spawn, format!("writing its input: {why}")); // alloc: an error.
                    return Step::Ends(None);
                }
                match self.progress(id, res) < len {
                    true => Step::Again,
                    false => Step::Ends(None),
                }
            }
            Kind::Accept => self.accepted(hot, res, flags & CQE_F_MORE != 0),
            Kind::Connect => {
                if res < 0 {
                    return Step::Ends(failed(res));
                }
                let connecting =
                    self.connectings.get_mut(Handle::from_bits(hot.of)).expect("a connect's connecting");
                let socket = connecting.socket.take().expect("a connect's socket");
                let tcp = connecting.tcp;
                let conn = self.add(socket, tcp);
                Step::Ends(Some(Done::Connected { conn }))
            }
            Kind::Recv => self.received(hot, res, flags),
            Kind::Wake => {
                self.take_offloaded();
                if res >= 0 && self.offload.out > 0 {
                    return Step::Again;
                }
                // Nothing more is off the ring, or the read failed: what is still out is taken
                // in at the polls that follow, and the next work off the ring reads again.
                self.offload.reading = None;
                Step::Ends(None)
            }
            Kind::Send => self.sent(id, hot, res, flags),
            Kind::Shutdown if res < 0 && res != -libc::ENOTCONN => Step::Ends(failed(res)),
            Kind::Shutdown => Step::Ends(Some(Done::HungUp)),
        }
    }

    /// The read of the watch `hot` completed with `res`.
    fn watched(&mut self, hot: Hot, res: i32) -> Step {
        let tag = hot.tag;
        let Uring { watchings, ready, watches, .. } = self;
        let Watching { tree, buffer, stopping, flushes, journal } =
            watchings.get_mut(Handle::from_bits(hot.of)).expect("a watch's read's watching");
        match res {
            // Cancelled for a flush, or by a flush's cancel that came after the read it was for.
            _ if res == -libc::ECANCELED && !*stopping => {
                flushed(ready, tag, tree, buffer, flushes, journal.as_ref());
                Step::Again
            }
            _ if res == -libc::ECANCELED || (res < 0 && *stopping) => {
                watches.remove(&tag);
                unflushed(ready, flushes);
                Step::Ends(Some(Done::Unwatched))
            }
            1.. => {
                let done = told(journal.as_ref(), tree.take(&buffer[..res as usize]));
                ready.push(Completion { tag, done });
                if !*stopping {
                    flushed(ready, tag, tree, buffer, flushes, journal.as_ref());
                    return Step::Again;
                }
                // The read was done before the cancel reached it: this change, then the end.
                watches.remove(&tag);
                unflushed(ready, flushes);
                Step::Ends(Some(Done::Unwatched))
            }
            _ => {
                watches.remove(&tag);
                unflushed(ready, flushes);
                Step::Ends(failed(if res < 0 { res } else { -libc::EIO }))
            }
        }
    }

    /// The accept of the listen `hot` completed with `res`; `more` where the kernel goes on
    /// accepting with it.
    fn accepted(&mut self, hot: Hot, res: i32, more: bool) -> Step {
        let tag = hot.tag;
        let stopping = hot.flags & STOPPING != 0;
        if res >= 0 {
            let tcp = self.listeners.get(Handle::from_bits(hot.of)).is_some_and(|it| !it.is_unix());
            let socket = self.table.accepted(res as u32);
            let conn = self.add(socket, tcp);
            let done = Done::Accepted { conn };
            // Stopping, it ends only at the completion that says the kernel let go of it: until
            // then it may accept more.
            if !stopping || more {
                return Step::Goes(done);
            }
            self.listens.remove(&tag);
            self.ready.push(Completion { tag, done });
            return Step::Ends(Some(Done::Unlistened));
        }
        if more {
            // The kernel goes on accepting with it: it has not ended.
            return Step::Held;
        }
        if res == -libc::ECANCELED || stopping {
            self.listens.remove(&tag);
            return Step::Ends(Some(Done::Unlistened));
        }
        if res == -libc::ENFILE {
            self.listens.remove(&tag);
            // alloc: an error.
            let why = format!(
                "{}: every one of the {} slots the ring's table of files keeps for accepted \
                 connections is in use",
                error(res),
                self.table.ours
            );
            return Step::Ends(Some(Done::Failed(IoError(why))));
        }
        // A connection that went before it was taken, or the network's errors that accept(2)
        // says to take as "try again".
        let again = [
            libc::ECONNABORTED,
            libc::EINTR,
            libc::EPROTO,
            libc::ENETDOWN,
            libc::ENOPROTOOPT,
            libc::EHOSTDOWN,
            libc::ENONET,
            libc::EHOSTUNREACH,
            libc::EOPNOTSUPP,
            libc::ENETUNREACH,
        ];
        if again.contains(&-res) {
            return Step::Again;
        }
        self.listens.remove(&tag);
        Step::Ends(failed(res))
    }

    /// The multishot receive `hot` completed with `res`, its bytes in the provided buffer its
    /// `flags` name.
    fn received(&mut self, hot: Hot, res: i32, flags: u32) -> Step {
        let conn = Conn(hot.of);
        let more = flags & CQE_F_MORE != 0;
        if flags & CQE_F_BUFFER != 0 {
            let bid = (flags >> CQE_BUFFER_SHIFT) as u16;
            let Uring { provided, conns, ready, .. } = self;
            if res > 0
                && let Some(connection) = conns.get_mut(conn_of(conn))
                && !connection.hung
            {
                take_in(connection, provided.bytes(bid, res as usize), ready);
            }
            provided.give(bid);
        }
        let Some(connection) = self.conns.get_mut(conn_of(conn)) else {
            return if more { Step::Held } else { Step::Ends(None) };
        };
        if !more {
            connection.receiving = None;
            connection.pausing = false;
            match res {
                // The peer hung up, or reset it: as good as hung up, for a reader.
                0 => connection.ended = Some(Ok(())),
                _ if res == -libc::ECONNRESET => connection.ended = Some(Ok(())),
                // Out of buffers, or paused: armed again once it is wanted.
                _ if res == -libc::ENOBUFS || res == -libc::ECANCELED || res > 0 => {}
                _ => connection.ended = Some(Err(error(res))),
            }
            return Step::Ends(None);
        }
        if connection.inbox.len() >= INBOX && !connection.pausing {
            connection.pausing = true;
            let id = connection.receiving.expect("an armed receive");
            self.cancel(id);
        }
        self.deliver(conn);
        Step::Held
    }

    /// The send `id` completed with `res`. A zero-copy one ends once its last send's result and
    /// every notification are in, the kernel then done with the asker's pages.
    fn sent(&mut self, id: Id, hot: Hot, res: i32, flags: u32) -> Step {
        let len = self.ops.buf(id).len() as u64;
        if hot.flags & ZERO_COPY == 0 {
            if res < 0 {
                return self.unsent(id, -res);
            }
            return match (self.progress(id, res) < len as usize, res) {
                (false, _) => {
                    // Its bytes are the asker's again: the kernel reads them no more.
                    let mut bytes = mem::take(self.ops.buf_mut(id));
                    bytes.clear();
                    Step::Ends(Some(Done::Sent(bytes)))
                }
                (true, 0) => self.unsent(id, libc::EIO),
                (true, _) => Step::Again,
            };
        }
        let hot = self.ops.get_mut(id).expect("a request in flight");
        let mut errno = 0;
        if flags & CQE_F_NOTIF != 0 {
            // Counted with wrapping, so a notification read before its send's result (which the
            // kernel does not do) would still balance.
            hot.notifs = hot.notifs.wrapping_sub(1);
        } else {
            hot.notifs = hot.notifs.wrapping_add(u16::from(flags & CQE_F_MORE != 0));
            if res > 0 {
                hot.done += res as u64;
            }
            errno = match res {
                _ if res < 0 => -res,
                0 if hot.done < len => libc::EIO,
                _ => 0,
            };
            if errno == 0 && hot.done < len {
                return Step::Again;
            }
            hot.flags |= FINISHED;
        }
        let (finished, notifs) = (hot.flags & FINISHED != 0, hot.notifs);
        if errno != 0 {
            self.ops.cold_mut(id).results[0] = errno;
        }
        if !finished || notifs != 0 {
            return Step::Held;
        }
        self.zero_copy_sends += 1;
        match self.ops.cold(id).results[0] {
            0 => {
                let mut bytes = mem::take(self.ops.buf_mut(id));
                bytes.clear();
                Step::Ends(Some(Done::Sent(bytes)))
            }
            errno => self.unsent(id, errno),
        }
    }

    /// The send `id` ends failed with `errno`, its buffer handed back cleared.
    fn unsent(&mut self, id: Id, errno: i32) -> Step {
        let bytes = mem::take(self.ops.buf_mut(id));
        Step::Ends(Some(Done::unsent(bytes, io::Error::from_raw_os_error(errno))))
    }

    /// Stops the watch or listen whose request is `id`: through the ring, or here if the
    /// request has not reached the kernel yet.
    pub(super) fn stop(&mut self, id: Id) {
        let Some(hot) = self.ops.get_mut(id) else { return };
        let (tag, kind, of) = (hot.tag, hot.kind, hot.of);
        match kind {
            Kind::Watch => {
                let watching = self.watchings.get_mut(Handle::from_bits(of)).expect("a watch's read's watching");
                if mem::replace(&mut watching.stopping, true) {
                    return;
                }
            }
            Kind::Accept => {
                if hot.flags & STOPPING != 0 {
                    return;
                }
                hot.flags |= STOPPING;
            }
            _ => unreachable!("a watch's or a listen's request"),
        }
        let Some(at) = self.waiting.iter().position(|&it| it == id) else {
            self.cancel(id);
            return;
        };
        self.waiting.remove(at);
        let done = match kind {
            Kind::Watch => {
                self.watches.remove(&tag);
                let watching = self.watchings.get_mut(Handle::from_bits(of)).expect("a watch's read's watching");
                unflushed(&mut self.ready, &mut watching.flushes);
                Done::Unwatched
            }
            _ => {
                self.listens.remove(&tag);
                Done::Unlistened
            }
        };
        self.ready.push(Completion { tag, done });
        self.end(id);
    }

    /// Asks the watch `watch` for the flush `tag`. Its read, run by the ring's kernel thread,
    /// would race a read of the descriptor here, so a read in the kernel is cancelled and the
    /// flush answered once its completion is in ([`flushed`]); a read still waiting for room
    /// in the ring reads nothing, so the descriptor is drained now.
    pub(super) fn flush(&mut self, watch: Tag, tag: Tag) -> io::Result<()> {
        let Some(&id) = self.watches.get(&watch) else {
            // alloc: an error path.
            return Err(io::Error::other(format!("nothing is watched as {watch:?}")));
        };
        let waiting = self.waiting.contains(&id);
        let hot = self.ops.get(id).expect("a watch's read");
        debug_assert_eq!(hot.kind, Kind::Watch);
        let Watching { tree, buffer, stopping, flushes, journal } =
            self.watchings.get_mut(Handle::from_bits(hot.of)).expect("a watch's read's watching");
        if *stopping {
            return Err(io::Error::other("the watch ended before the flush"));
        }
        flushes.push(tag);
        if waiting {
            flushed(&mut self.ready, watch, tree, buffer, flushes, journal.as_ref());
            return Ok(());
        }
        if flushes.len() == 1 {
            self.cancel(id);
        }
        Ok(())
    }

    pub(super) fn spawn(&mut self, tag: Tag, command: Box<Command>) -> io::Result<()> {
        let crate::process::Started { pid, pidfd, stdin, stdout, stderr, mut finish } =
            crate::process::start(&self.spawn_dirs, command, self.confine.as_ref())?;
        let input = mem::take(&mut finish.command.stdin);
        let parts = 3 + usize::from(stdin.is_some());
        let spawn = self.spawns.insert(Spawning {
            tag,
            pid,
            pidfd,
            finish,
            stdout: Vec::new(), // alloc: empty until the process writes; the spawn's own output.
            stderr: Vec::new(), // alloc: empty until the process writes; the spawn's own output.
            parts,
            exited: false,
            failed: None,
        });
        self.spawning.insert(tag, spawn);
        let of = spawn.to_bits();
        self.start(Hot::new(tag, Kind::Exit, of), ops::NO_BYTES, Cold::EMPTY);
        let buffer = self.buffer();
        self.start(Hot::new(tag, Kind::Output, of), buffer, Cold { fds: [Some(stdout), None], ..Cold::EMPTY });
        let buffer = self.buffer();
        let mut hot = Hot::new(tag, Kind::Output, of);
        hot.flags |= STDERR;
        self.start(hot, buffer, Cold { fds: [Some(stderr), None], ..Cold::EMPTY });
        if let Some(fd) = stdin {
            self.start(Hot::new(tag, Kind::Input, of), input, Cold { fds: [Some(fd), None], ..Cold::EMPTY });
        }
        Ok(())
    }

    fn add(&mut self, socket: Fixed, tcp: bool) -> Conn {
        let conn = self.conns.insert(Connection {
            socket,
            users: 0,
            hung: false,
            tcp,
            inbox: VecDeque::new(), // alloc: empty until it receives; grown once and reused.
            recvs: VecDeque::new(), // alloc: empty until it is asked; grown once and reused.
            receiving: None,
            pausing: false,
            ended: None,
        });
        Conn(conn.to_bits())
    }

    /// Hands `conn`'s waiting `Recv`s what its inbox holds, or its end, and arms its receive
    /// where one waits for bytes not yet taken.
    pub(super) fn deliver(&mut self, conn: Conn) {
        let Some(connection) = self.conns.get_mut(conn_of(conn)) else { return };
        while let Some((tag, most, into)) = connection.recvs.front_mut() {
            let (tag, most) = (*tag, *most);
            let done = match &connection.ended {
                _ if most == 0 => Done::Read(mem::take(into)),
                _ if !connection.inbox.is_empty() => {
                    let n = most.min(RECV_MOST).min(connection.inbox.len());
                    let (front, back) = connection.inbox.as_slices();
                    let split = front.len().min(n);
                    // alloc: none once the asker's buffer has room for `n` more.
                    into.extend_from_slice(&front[..split]);
                    into.extend_from_slice(&back[..n - split]);
                    connection.inbox.drain(..n);
                    Done::Read(mem::take(into))
                }
                Some(Ok(())) => Done::Read(mem::take(into)),
                Some(Err(why)) => Done::unread(mem::take(into), why),
                None => break,
            };
            connection.recvs.pop_front();
            self.ready.push(Completion { tag, done });
        }
        let arm = connection.receiving.is_none()
            && connection.ended.is_none()
            && !connection.hung
            && !connection.recvs.is_empty()
            && connection.inbox.len() < INBOX;
        if !arm {
            return;
        }
        connection.users += 1;
        let id = self.start(Hot::new(NOBODYS, Kind::Recv, conn.0), ops::NO_BYTES, Cold::EMPTY);
        if let Some(connection) = self.conns.get_mut(conn_of(conn)) {
            connection.receiving = Some(id);
        }
    }

    /// Shuts `conn` both ways through the ring, its waiting receives answered empty, and lets
    /// it go once nothing in flight uses it.
    pub(super) fn hang_up(&mut self, conn: Conn, tag: Tag) -> io::Result<()> {
        let connection = self.conns.get_mut(conn_of(conn)).filter(|it| !it.hung).ok_or_else(|| gone(conn))?;
        connection.hung = true;
        connection.inbox.clear();
        connection.users += 1;
        let receiving = connection.receiving;
        for (tag, _, into) in connection.recvs.drain(..) {
            self.ready.push(Completion { tag, done: Done::Read(into) });
        }
        if let Some(id) = receiving {
            self.cancel(id);
        }
        self.start(Hot::new(tag, Kind::Shutdown, conn.0), ops::NO_BYTES, Cold::EMPTY);
        Ok(())
    }

    /// Starts the watch `tag` of `dir`, into the tables of one that ended where there is one;
    /// an [`Effect::Journal`]'s with `journal`.
    pub(super) fn watch(&mut self, tag: Tag, dir: FileName, journal: bool) -> io::Result<()> {
        let (spare, flushes) = match self.trees.pop() {
            Some((spare, flushes)) => (Some(spare), flushes),
            None => (None, Vec::new()), // alloc: empty until a flush; kept for the next watch.
        };
        let tree = crate::watch::Tree::reusing(spare, &self.root, &dir, true)?;
        let buffer = self.buffer();
        let journal = journal.then_some(dir);
        let watch = self.watchings.insert(Watching { tree, buffer, stopping: false, flushes, journal });
        let id = self.start(Hot::new(tag, Kind::Watch, watch.to_bits()), ops::NO_BYTES, Cold::EMPTY);
        self.watches.insert(tag, id);
        Ok(())
    }

    /// Sends that went from the asker's pages and ended, since the driver was made.
    pub fn zero_copy_sends(&self) -> u64 {
        self.zero_copy_sends
    }
}

/// `bytes` received on `connection`: straight into the buffers of the receives waiting, in
/// order, while nothing received before waits in its inbox, so most bytes are copied once
/// (from the provided buffer to the asker's); the rest into its inbox, for receives to come.
fn take_in(connection: &mut Connection, mut bytes: &[u8], ready: &mut Vec<Completion>) {
    while connection.inbox.is_empty()
        && !bytes.is_empty()
        && let Some((tag, most, into)) = connection.recvs.front_mut()
        && *most > 0
    {
        let n = (*most).min(RECV_MOST).min(bytes.len());
        // alloc: none once the asker's buffer has room for `n` more.
        into.extend_from_slice(&bytes[..n]);
        bytes = &bytes[n..];
        let (tag, into) = (*tag, mem::take(into));
        connection.recvs.pop_front();
        ready.push(Completion { tag, done: Done::Read(into) });
    }
    // alloc: none once the inbox has grown to what is received ahead of its asking.
    connection.inbox.extend(bytes);
}

impl Drop for Uring {
    /// Kills the processes of spawns in flight and takes back from the kernel every buffer
    /// it was lent (every request cancelled, its completions waited for), so none is freed
    /// while the kernel may still read or write it. What it does not give back in time is
    /// leaked, with the provided buffers its receives write into.
    fn drop(&mut self) {
        for (_, spawn) in self.spawns.iter() {
            if !spawn.exited {
                _ = kill(spawn.pidfd.as_raw_fd());
            }
        }
        self.waiting.clear();
        self.on_open.clear();
        self.slot_waiting.clear();
        self.load_waiting.clear();
        drop(self.lent.clear_waiting());
        // alloc: drop, once.
        let mut owed: HashMap<Id, usize> = self.ops.iter().map(|(id, hot)| (id, owed(hot))).collect();
        // Those the kernel holds nothing of: never handed to it, or every completion in.
        owed.retain(|&id, left| {
            if *left == 0 {
                self.ops.remove(id);
            }
            *left > 0
        });
        let deadline = Instant::now() + LETTING_GO;
        let mut posted = Vec::new(); // alloc: the driver's drop, once.
        let cancel = Sqe {
            opcode: OP_ASYNC_CANCEL,
            fd: -1,
            op_flags: ASYNC_CANCEL_ALL | ASYNC_CANCEL_ANY,
            user_data: NOBODY,
            ..Sqe::default()
        };
        let mut alive = true;
        while alive && !self.ops.is_empty() && !self.ring.push(&[cancel]) {
            alive = Instant::now() < deadline && self.ring.enter(None).is_ok();
            self.ring.reap(|user_data, res, flags| posted.push((user_data, res, flags)));
        }
        while alive {
            for (user_data, res, flags) in posted.drain(..) {
                let (id, _) = of_data(user_data);
                match self.ops.get(id).map(|it| it.kind) {
                    // A connection the accept made after it was cancelled, let go here.
                    Some(Kind::Accept) if res >= 0 => _ = self.table.set(res as u32, -1),
                    Some(Kind::Recv) if flags & CQE_F_BUFFER != 0 => {
                        self.provided.give((flags >> CQE_BUFFER_SHIFT) as u16);
                    }
                    _ => {}
                }
                // A multishot request the kernel still holds is not given back yet.
                if flags & CQE_F_MORE == 0
                    && let Some(left) = owed.get_mut(&id)
                    && *left > 0
                {
                    *left -= 1;
                    if *left == 0 {
                        // A connection's or a spawn's request let go: what it used goes with
                        // the driver below.
                        self.ops.remove(id);
                    }
                }
            }
            self.provided.publish();
            let left = deadline.saturating_duration_since(Instant::now());
            let lent = self.ops.iter().any(|(_, it)| it.kind != Kind::Cancel);
            if !lent || left.is_zero() {
                break;
            }
            alive = self.ring.enter(Some(left)).is_ok();
            self.ring.reap(|user_data, res, flags| posted.push((user_data, res, flags)));
        }
        self.spawning.clear();
        // alloc: the driver's drop, once.
        let spawns: Vec<Handle<Spawning>> = self.spawns.iter().map(|(it, _)| it).collect();
        for spawn in spawns {
            _ = self.spawns.remove(spawn).expect("listed").end();
        }
        if self.ops.iter().any(|(_, it)| it.kind != Kind::Cancel) {
            return;
        }
        // SAFETY: dropped once, here; no request the kernel holds writes into a provided buffer
        // any more (the check above). Where one does, it is leaked with what it uses.
        unsafe { ManuallyDrop::drop(&mut self.provided) };
        // SAFETY: as above: no request the kernel holds reads or writes a buffer, address or
        // cold record in the columns.
        unsafe { ManuallyDrop::drop(&mut self.ops) };
        // SAFETY: as above: no rename the kernel holds reads a name.
        unsafe { ManuallyDrop::drop(&mut self.paths) };
        // SAFETY: as above: no watch's read the kernel holds writes into a watching's buffer.
        unsafe { ManuallyDrop::drop(&mut self.watchings) };
        // SAFETY: as above: no connect the kernel holds reads an address.
        unsafe { ManuallyDrop::drop(&mut self.connectings) };
        // SAFETY: as above: no read of the wake the kernel holds writes into the count.
        unsafe { ManuallyDrop::drop(&mut self.offload.count) };
    }
}
