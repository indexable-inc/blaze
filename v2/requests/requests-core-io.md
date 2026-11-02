# Requests from requests-core-io

blaze-core applied v2/requests/usercode-hot-pm.md section 1. Its socket effects changed:

- `Effect::Recv { conn, most, into: Vec<u8>, tag }`: at most `most` bytes appended to `into`
  after what it holds (empty, or bytes the asker keeps, such as a frame's start), and at most
  the new `blaze_core::RECV_MOST` (64 KiB, the bound kqueue and Windows had) on every driver:
  the room is reserved before the bytes come. Its capacity never shrinks and grows to
  `into.len() + most.min(RECV_MOST)` at most. An asker of a longer frame asks again, as it did
  on kqueue and Windows; on the threads driver and in the simulation a receive used to return up
  to all of `most` at once.
- `Done::Read(into)`: the receive's buffer back, nothing appended once the peer hung up.
- `Done::Sent(bytes)`: the send's buffer back, cleared, its capacity kept (was `Done::Sent`).
- `Done::Broken { buffer, error: IoError }` (new): a send or receive that failed, its buffer
  back (a send's cleared, a receive's as given). It replaces `Done::Failed` for `Send` and
  `Recv`, including "no such connection" at submit. **Every caller that matched
  `Done::Failed(_)` on a send's or receive's completion must match `Done::Broken` now**, or a
  broken connection falls into its catch-all arm.

Every hunk below is old text, then new text, in the owner's file. A caller that kept no buffer
before gets the minimal change (`into: Vec::new()`, `Done::Sent(_)`); one on a hot path also
gets the reuse that makes it allocate nothing, as noted.

## 1. proc-macro-api (`crates/proc-macro-api/src/warm.rs`, owner usercode)

The caller this change is for: `Bridge` carries each buffer back, `LinkWriter` and `LinkReader`
each keep theirs, so a warm expansion's conversation allocates no frame either way.
`Link::send` hands the cleared buffer back; `Link::recv` takes the buffer to append to.

### 1a. `Link::send`/`Link::recv`

```rust
    /// Sends all of `bytes`.
    pub fn send(&self, bytes: Vec<u8>) -> io::Result<()> {
        match self {
            Link::Bridge(it) => it.send(bytes),
            #[cfg(test)]
            Link::Script(it) => it.send(bytes),
        }
    }

    /// What the server sent since the last receive, at most `most` bytes: empty once it hung up.
    pub fn recv(&self, most: usize) -> io::Result<Vec<u8>> {
        match self {
            Link::Bridge(it) => it.recv(most),
            #[cfg(test)]
            Link::Script(it) => it.recv(most),
        }
    }
```

new:

```rust
    /// Sends all of `bytes`; their buffer back, cleared, its capacity kept for the next.
    pub fn send(&self, bytes: Vec<u8>) -> io::Result<Vec<u8>> {
        match self {
            Link::Bridge(it) => it.send(bytes),
            #[cfg(test)]
            Link::Script(it) => it.send(bytes),
        }
    }

    /// `into` with what the server sent since the last receive appended, at most `most` bytes:
    /// nothing appended once it hung up.
    pub fn recv(&self, into: Vec<u8>, most: usize) -> io::Result<Vec<u8>> {
        match self {
            Link::Bridge(it) => it.recv(into, most),
            #[cfg(test)]
            Link::Script(it) => it.recv(into, most),
        }
    }
```

### 1b. `LinkWriter::flush`

```rust
        // The frame the driver takes, as `Effect::Send` owns its bytes: one of its exact length,
        // the buffer kept for the next (v2/requests/usercode-hot-pm.md asks blaze-core to hand
        // sent buffers back).
        // alloc: the edge, one per frame, as above.
        let bytes = self.buffer.to_vec();
        self.buffer.clear();
        self.link.send(bytes).map_err(|error| broken(&self.link, error))
```

new:

```rust
        // The buffer goes to the driver with the frame and comes back cleared, its capacity
        // kept for the next.
        let bytes = std::mem::take(&mut self.buffer);
        self.buffer = self.link.send(bytes).map_err(|error| broken(&self.link, error))?;
        Ok(())
```

### 1c. `LinkReader::new` and `fill_buf`

```rust
        // alloc: none; each receive's bytes are the driver's, read where they are.
        LinkReader { link, buffer: Vec::new(), at: 0 }
```

new:

```rust
        // alloc: none until the first receive; each receive appends to the buffer the last
        // gave back, kept as long as the longest.
        LinkReader { link, buffer: Vec::new(), at: 0 }
```

```rust
        if self.at == self.buffer.len() {
            self.buffer = self.link.recv(RECV).map_err(|error| broken(&*self.link, error))?;
            self.at = 0;
        }
```

new:

```rust
        if self.at == self.buffer.len() {
            let mut into = std::mem::take(&mut self.buffer);
            into.clear();
            self.buffer = self.link.recv(into, RECV).map_err(|error| broken(&*self.link, error))?;
            self.at = 0;
        }
```

### 1d. `Ask` and `Answer`

```rust
enum Ask {
    Send(Vec<u8>),
    Recv(usize),
}

/// What a [`Bridge`] answers an ask with: nothing for a send, the bytes for a receive.
type Answer = io::Result<Vec<u8>>;
```

new:

```rust
enum Ask {
    Send(Vec<u8>),
    /// At most this many bytes, appended to the buffer.
    Recv(usize, Vec<u8>),
}

/// What a [`Bridge`] answers an ask with: the buffer back, cleared for a send, with the bytes
/// appended for a receive.
type Answer = io::Result<Vec<u8>>;
```

### 1e. `BridgeLink::send`/`recv`

```rust
    fn send(&self, bytes: Vec<u8>) -> io::Result<()> {
        self.ask(Ask::Send(bytes)).map(drop)
    }

    fn recv(&self, most: usize) -> io::Result<Vec<u8>> {
        self.ask(Ask::Recv(most))
    }
```

new:

```rust
    fn send(&self, bytes: Vec<u8>) -> io::Result<Vec<u8>> {
        self.ask(Ask::Send(bytes))
    }

    fn recv(&self, into: Vec<u8>, most: usize) -> io::Result<Vec<u8>> {
        self.ask(Ask::Recv(most, into))
    }
```

### 1f. `Bridge::effects` and `Bridge::complete`

```rust
                Ask::Send(bytes) => Effect::Send { conn: self.conn, bytes, tag },
                Ask::Recv(most) => Effect::Recv { conn: self.conn, most, tag },
```

new:

```rust
                Ask::Send(bytes) => Effect::Send { conn: self.conn, bytes, tag },
                Ask::Recv(most, into) => Effect::Recv { conn: self.conn, most, into, tag },
```

```rust
        let answered = match completion.done {
            // alloc: none, an empty `Vec`.
            Done::Sent => Ok(Vec::new()),
            Done::Read(bytes) => Ok(bytes),
            Done::Failed(error) => Err(io::Error::other(error.0)),
```

new:

```rust
        let answered = match completion.done {
            Done::Sent(bytes) | Done::Read(bytes) => Ok(bytes),
            Done::Broken { error, .. } | Done::Failed(error) => Err(io::Error::other(error.0)),
```

### 1g. `Script` (tests)

```rust
    fn send(&self, bytes: Vec<u8>) -> io::Result<()> {
        self.sent.lock().unwrap().extend(bytes);
        Ok(())
    }

    fn recv(&self, most: usize) -> io::Result<Vec<u8>> {
        let mut answers = self.answers.lock().unwrap();
        let Some(mut next) = answers.pop_front() else { return Ok(Vec::new()) };
        if next.len() > most {
            let rest = next.split_off(most);
            answers.push_front(rest);
        }
        Ok(next)
    }
```

new:

```rust
    fn send(&self, mut bytes: Vec<u8>) -> io::Result<Vec<u8>> {
        self.sent.lock().unwrap().extend_from_slice(&bytes);
        bytes.clear();
        Ok(bytes)
    }

    fn recv(&self, mut into: Vec<u8>, most: usize) -> io::Result<Vec<u8>> {
        let mut answers = self.answers.lock().unwrap();
        let Some(mut next) = answers.pop_front() else { return Ok(into) };
        if next.len() > most {
            let rest = next.split_off(most);
            answers.push_front(rest);
        }
        into.extend_from_slice(&next);
        Ok(into)
    }
```

### 1h. test `the_bridge_turns_calls_into_effects_and_completions_into_answers`

```rust
                link.send(b"hello".to_vec()).unwrap();
                link.recv(8).unwrap()
```

new:

```rust
                assert!(link.send(b"hello".to_vec()).unwrap().is_empty());
                link.recv(b"<".to_vec(), 8).unwrap()
```

```rust
                    Effect::Send { conn, bytes, .. } => {
                        assert_eq!((conn, bytes.as_slice()), (Conn(3), b"hello".as_slice()));
                        Done::Sent
                    }
                    Effect::Recv { conn, most, .. } => {
                        assert_eq!((conn, most), (Conn(3), 8));
                        Done::Read(b"world".to_vec())
                    }
```

new:

```rust
                    Effect::Send { conn, mut bytes, .. } => {
                        assert_eq!((conn, bytes.as_slice()), (Conn(3), b"hello".as_slice()));
                        bytes.clear();
                        Done::Sent(bytes)
                    }
                    Effect::Recv { conn, most, mut into, .. } => {
                        assert_eq!((conn, most, into.as_slice()), (Conn(3), 8, b"<".as_slice()));
                        into.extend_from_slice(b"world");
                        Done::Read(into)
                    }
```

```rust
        assert_eq!(thread.join().unwrap(), b"world");
```

new:

```rust
        assert_eq!(thread.join().unwrap(), b"<world");
```

### 1i. test `an_ask_through_the_bridge_allocates_nothing_on_the_asking_thread`

```rust
    /// Once its channels have carried an ask, an ask allocates nothing on the thread that
    /// makes it: the bytes it sends were made before, the bytes it receives the driver made.
```

new:

```rust
    /// Once its channels have carried an ask, an ask allocates nothing on the thread that
    /// makes it: each buffer it hands the driver comes back for the next.
```

```rust
                for round in 0..3 {
                    let bytes = b"ask".to_vec();
                    let (got, used) = blaze_alloc_count::measure(|| {
                        link.send(bytes).unwrap();
                        link.recv(8).unwrap()
                    });
                    assert_eq!(got, b"answer");
```

new:

```rust
                let (mut sending, mut receiving) = (Vec::with_capacity(8), Vec::with_capacity(8));
                for round in 0..3 {
                    sending.extend_from_slice(b"ask");
                    receiving.clear();
                    let ((), used) = blaze_alloc_count::measure(|| {
                        sending = link.send(std::mem::take(&mut sending)).unwrap();
                        receiving = link.recv(std::mem::take(&mut receiving), 8).unwrap();
                    });
                    assert_eq!((sending.len(), &receiving[..]), (0, &b"answer"[..]));
```

```rust
                    Effect::Send { .. } => Done::Sent,
                    Effect::Recv { .. } => Done::Read(b"answer".to_vec()),
```

new:

```rust
                    Effect::Send { mut bytes, .. } => {
                        bytes.clear();
                        Done::Sent(bytes)
                    }
                    Effect::Recv { mut into, .. } => {
                        into.extend_from_slice(b"answer");
                        Done::Read(into)
                    }
```

> applied (usercode, 2026-10-29), not built: 1a to 1i as written.

## 2. blaze-api (`crates/blaze-api/src/server.rs`, `server/tests.rs`, owner ci)

> applied by requests-core-io (requests pass 31: lane ci DONE): 2a-2d as written; the warm test
> also hands each send's real buffer back, asserts a warm Unix reply allocates 0 (HTTP, whose
> reply is written while its head's send is in flight, keeps <= 1), and is renamed
> `a_warm_request_and_its_reply_reuse_their_buffers`.

Receives keep `Frames::push_vec` taking the bytes, so they hand an empty buffer. A send's
buffer comes back to the connection's queue, so a warm reply no longer allocates its queue.

### 2a. `Server::receive` (server.rs)

```rust
        effects.push(Effect::Recv { conn, most, tag });
```

new:

```rust
        // alloc: the room the driver reserves, which the frames then keep (`push_vec`).
        effects.push(Effect::Recv { conn, most, into: Vec::new(), tag });
```

### 2b. `Server::sent` (server.rs)

```rust
        connection.sending = None;
        if done != Done::Sent {
            return self.gone(conn, effects);
        }
        if !connection.queue.is_empty() {
            let bytes = std::mem::take(&mut connection.queue);
            return self.send(conn, bytes, effects);
        }
```

new:

```rust
        connection.sending = None;
        let Done::Sent(spare) = done else {
            return self.gone(conn, effects);
        };
        // The buffer sent is the queue's next, its room kept.
        if !connection.queue.is_empty() {
            let bytes = std::mem::replace(&mut connection.queue, spare);
            return self.send(conn, bytes, effects);
        }
        if connection.queue.capacity() == 0 {
            connection.queue = spare;
        }
```

### 2c. `room` (server.rs)

```rust
        // alloc: one buffer for the replies queued until the next send, which takes it
        // (`Effect::Send` owns its bytes; v2/requests/core.md K3 would hand it back).
```

new:

```rust
        // alloc: one buffer for the replies queued until the next send, which takes it; the
        // send's end hands it back to the queue (`Server::sent`), so only the first allocates.
```

### 2d. server/tests.rs

```rust
            Effect::Recv { conn: of, most, tag } if *of == conn => {
```

new:

```rust
            Effect::Recv { conn: of, most, tag, .. } if *of == conn => {
```

`Done::Sent` becomes `Done::Sent(Vec::new())` at each of its three places: in `Rig::sent`
(`self.complete(tag, Done::Sent);`), in
`one_send_is_in_flight_and_later_replies_are_concatenated_behind_it`
(`rig.complete(sends[0].0, Done::Sent);`) and in
`a_warm_request_allocates_nothing_and_its_reply_one_buffer`
(`rig.server.complete(Completion { tag: sent, done: Done::Sent }, &mut effects, &mut arrived)`).
That last test's `replied.allocations <= 1` still holds; handing the real sent bytes back there
(`Effect::Send { bytes, .. }` taken out of `rig.in_flight`, cleared) would let it assert 0.

## 3. blaze-global (`crates/blaze-global/src/{server,client,tests}.rs`, owner not in crates.md: requests routes it)

> applied by requests-core-io (requests pass 31): 3a-3e as written, with `// alloc:` notes on
> the empty receive buffers and the error arms. No blaze-global file had changed since 22:06;
> fastcompile-sol-global (whose work touched `parse_fetch_ask` in server.rs) was told by tmux.

A send's buffer comes back as the next `staged`. Receives hand an empty buffer: `received`
only borrows the bytes, so a `Vec<u8>` field per peer and link, handed to each `Recv` and put
back from `Done::Read`, would make receives allocate nothing too (not written here: it adds a
field). `RECV_LEAST` (1 MiB) and `RECV_MOST` (64 MiB) now ask for more than one receive appends
(`blaze_core::RECV_MOST`, 64 KiB); the windows still work, with more receives per body.

### 3a. server.rs, `Server::complete`

```rust
            Some(Role::Send(at)) => match done {
                Done::Sent => {
                    self.peers[at].sending = None;
                    self.pump(at, store, effects);
                }
```

new:

```rust
            Some(Role::Send(at)) => match done {
                Done::Sent(spare) => {
                    let peer = &mut self.peers[at];
                    peer.sending = None;
                    // The buffer sent is the next staged one, its room kept.
                    if peer.staged.capacity() == 0 {
                        peer.staged = spare;
                    }
                    self.pump(at, store, effects);
                }
```

(Its `done =>` arms already name a `Done::Broken` in their message.)

### 3b. server.rs, `Server::accept` and `Server::pump`

```rust
        effects.push(Effect::Recv { conn, most: RECV_LEAST, tag });
```

new:

```rust
        effects.push(Effect::Recv { conn, most: RECV_LEAST, into: Vec::new(), tag });
```

```rust
            effects.push(Effect::Recv { conn: peer.conn, most: owed.clamp(RECV_LEAST, RECV_MOST), tag });
```

new:

```rust
            let most = owed.clamp(RECV_LEAST, RECV_MOST);
            effects.push(Effect::Recv { conn: peer.conn, most, into: Vec::new(), tag });
```

### 3c. client.rs, the completion of a send and of a receive

```rust
                Done::Sent => {
                    if let Link::Up(up) = &mut self.links[at] {
                        up.sending = None;
                    }
                    self.pump_all(&mut answers, effects);
                }
                Done::Failed(error) => self.drop_link(at, &format!("sending failed: {error}"), &mut answers, effects),
```

new:

```rust
                Done::Sent(spare) => {
                    if let Link::Up(up) = &mut self.links[at] {
                        up.sending = None;
                        // The buffer sent is the next staged one, its room kept.
                        if up.staged.capacity() == 0 {
                            up.staged = spare;
                        }
                    }
                    self.pump_all(&mut answers, effects);
                }
                Done::Broken { error, .. } | Done::Failed(error) => {
                    self.drop_link(at, &format!("sending failed: {error}"), &mut answers, effects)
                }
```

```rust
                    Done::Failed(error) => {
                        self.drop_link(at, &format!("receiving failed: {error}"), &mut answers, effects)
                    }
```

new:

```rust
                    Done::Broken { error, .. } | Done::Failed(error) => {
                        self.drop_link(at, &format!("receiving failed: {error}"), &mut answers, effects)
                    }
```

### 3d. client.rs, `pump`

```rust
            effects.push(Effect::Recv { conn: up.conn, most: owed.clamp(RECV_LEAST, RECV_MOST), tag });
```

new:

```rust
            let most = owed.clamp(RECV_LEAST, RECV_MOST);
            effects.push(Effect::Recv { conn: up.conn, most, into: Vec::new(), tag });
```

### 3e. tests.rs, `Raw::complete` (two places, the same text)

```rust
            effects.push(Effect::Recv { conn, most: 1 << 24, tag: recv });
```

new (both):

```rust
            effects.push(Effect::Recv { conn, most: 1 << 24, into: Vec::new(), tag: recv });
```

(the second one is indented four spaces less, `        effects.push(...)`).

## 4. blaze-usercode (`crates/blaze-usercode/src/test/queue.rs`, `test/queue/tests.rs`, owner usercode)

**A correctness fix, not only a type change**: `Queue` loses a replica only on
`Done::Failed` of a send; a failed send now completes `Done::Broken`, which without 4a would be
ignored and the replica's tests would wait forever. A send's buffer comes back as the replica's
outbox. Receives hand an empty buffer (`frames.push_owned` keeps the bytes).

### 4a. queue.rs, the completion of a send

```rust
            For::Send(b, r) => {
                if let Done::Failed(error) = completion.done {
                    self.lose(b as usize, r, format!("could not be sent to: {error}"), effects); // alloc: error path
                }
            }
```

new:

```rust
            For::Send(b, r) => match completion.done {
                // The buffer sent is the replica's outbox again, its room kept.
                Done::Sent(spare) => {
                    if let Some(server) = self.bins[b as usize].server.as_mut()
                        && let Some(replica) = server.replicas.get_mut(r as usize)
                        && replica.outbox.capacity() == 0
                    {
                        replica.outbox = spare;
                    }
                }
                Done::Broken { error, .. } | Done::Failed(error) => {
                    self.lose(b as usize, r, format!("could not be sent to: {error}"), effects); // alloc: error path
                }
                _ => {}
            },
```

### 4b. queue.rs, `Queue::received`

```rust
            Done::Read(bytes) => bytes,
            Done::Failed(error) => {
                self.lose(b, r, format!("could not be read from: {error}"), effects); // alloc: error path
```

new:

```rust
            Done::Read(bytes) => bytes,
            Done::Broken { error, .. } | Done::Failed(error) => {
                self.lose(b, r, format!("could not be read from: {error}"), effects); // alloc: error path
```

### 4c. queue.rs, the two receives

```rust
                effects.push(Effect::Recv { conn, most: RECV, tag: recv });
```

new:

```rust
                effects.push(Effect::Recv { conn, most: RECV, into: Vec::new(), tag: recv });
```

```rust
            effects.push(Effect::Recv { conn, most, tag: recv });
```

new:

```rust
            effects.push(Effect::Recv { conn, most, into: Vec::new(), tag: recv });
```

### 4d. queue/tests.rs, `Fake::pump`

```rust
                    Effect::Send { conn, bytes, tag } => {
                        let mut frames = Frames::new();
                        frames.push(&bytes);
                        while let Some(frame) = frames.pop().unwrap() {
                            self.sent.push((conn, frame));
                        }
                        self.sends += 1;
                        answers.push(Completion { tag, done: Done::Sent });
                    }
```

new:

```rust
                    Effect::Send { conn, mut bytes, tag } => {
                        let mut frames = Frames::new();
                        frames.push(&bytes);
                        while let Some(frame) = frames.pop().unwrap() {
                            self.sent.push((conn, frame));
                        }
                        self.sends += 1;
                        bytes.clear();
                        answers.push(Completion { tag, done: Done::Sent(bytes) });
                    }
```

> applied (usercode, 2026-10-29), not built. 4a to 4c as written; 4d by hand, since the
> fake's text differed only in whitespace. Added for 4a: the fake can break the sends to chosen
> connections (`Fake::breaks`), and the test `a_replica_a_send_to_breaks_is_lost` checks that a
> `Done::Broken` send loses that replica's test with why, sends it nothing more and leaves the
> other replicas running. The queue's other `Done::Failed` arms (spawn, write, record read,
> listen) aren't sends or receives and stay as they are.

## 5. blaze-cas (`crates/blaze-cas/src/remote.rs`, owner store)

> applied by requests-core-io (requests pass 31: lane store DONE): 5a-5f as written, on top of
> the earlier move of `Tags` into blaze-core.

The client's send buffer comes back as `self.out`. Receives hand an empty buffer. The client
appends each `Done::Read` onto `up.buffer`; handing `std::mem::take(&mut up.buffer)` as `into`
and taking `Done::Read(bytes)` back as `up.buffer` would drop that copy too, but the tests'
`Net` fake hands back chunks of its own and would then have to append to the `into` it was
given. That is the owner's call; the hunks below are only the exact change. `lib.rs` needs
nothing: its `Done::Read`s are file reads and the remote tier's own answers.

### 5a. `Client::complete`, a send

```rust
                Done::Sent => {
                    if let Link::Up(up) = &mut self.link {
                        up.sending = None;
                    }
                    self.pump(effects);
                }
                Done::Failed(error) => {
                    self.fail(&format!("sending asks failed: {error}"), answers, effects)
                }
```

new:

```rust
                Done::Sent(spare) => {
                    if let Link::Up(up) = &mut self.link {
                        up.sending = None;
                    }
                    // The buffer sent holds the next asks, its room kept.
                    if self.out.capacity() == 0 {
                        self.out = spare;
                    }
                    self.pump(effects);
                }
                Done::Broken { error, .. } | Done::Failed(error) => {
                    self.fail(&format!("sending asks failed: {error}"), answers, effects)
                }
```

### 5b. `Client::complete`, a receive

```rust
                    Done::Failed(error) => {
                        self.fail(&format!("receiving answers failed: {error}"), answers, effects)
                    }
```

new:

```rust
                    Done::Broken { error, .. } | Done::Failed(error) => {
                        self.fail(&format!("receiving answers failed: {error}"), answers, effects)
                    }
```

### 5c. `Client::pump` and `Server::pump`

```rust
            effects.push(Effect::Recv { conn: up.conn, most, tag });
```

new:

```rust
            effects.push(Effect::Recv { conn: up.conn, most, into: Vec::new(), tag });
```

```rust
            effects.push(Effect::Recv { conn, most: SERVER_RECV, tag });
```

new:

```rust
            effects.push(Effect::Recv { conn, most: SERVER_RECV, into: Vec::new(), tag });
```

### 5d. `Server::complete`, a send

```rust
            match done {
                Done::Sent => {
                    if let Some((_, len)) = peer.sending.take() {
```

new:

```rust
            match done {
                Done::Sent(_) => {
                    if let Some((_, len)) = peer.sending.take() {
```

### 5e. tests: the `Net` fake

```rust
                Effect::Send { conn, bytes, tag } => {
                    let link = self.end(side, conn);
                    let sent = if link[side].open && link[1 - side].open {
                        link[1 - side].incoming.extend_from_slice(&bytes);
                        Done::Sent
                    } else {
                        Done::Failed(IoError("the peer hung up".into()))
                    };
                    self.inbox[side].push_back(done(tag, sent));
                }
                Effect::Recv { conn, most, tag } => {
```

new:

```rust
                Effect::Send { conn, mut bytes, tag } => {
                    let link = self.end(side, conn);
                    let open = link[side].open && link[1 - side].open;
                    if open {
                        link[1 - side].incoming.extend_from_slice(&bytes);
                    }
                    bytes.clear();
                    let sent = match open {
                        true => Done::Sent(bytes),
                        false => Done::Broken { buffer: bytes, error: IoError("the peer hung up".into()) },
                    };
                    self.inbox[side].push_back(done(tag, sent));
                }
                Effect::Recv { conn, most, tag, .. } => {
```

### 5f. tests: two completions of a send

```rust
        assert!(completed(&mut client, Completion { tag: send, done: Done::Sent }, &mut effects).is_empty());
```

new:

```rust
        assert!(completed(&mut client, Completion { tag: send, done: Done::Sent(Vec::new()) }, &mut effects).is_empty());
```

```rust
            let done = Completion { tag: send, done: Done::Sent };
```

new:

```rust
            let done = Completion { tag: send, done: Done::Sent(Vec::new()) };
```
