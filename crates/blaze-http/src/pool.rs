//! The idle connections between requests, keyed by [`Origin`]: a connection to one origin is
//! never used for another. It holds at most `most`, each for at most `keep` (the first pass's
//! `Pool`, hyper-util's defaults: 64 and 90 seconds), the one idle longest going first when it
//! is full. It knows connections by id only; the [`crate::Client`] closes what it hands back.

#![blaze::sans_io]

use std::{collections::VecDeque, time::Duration};

use crate::client::ConnId;

/// Where a connection goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Origin {
    pub(crate) https: bool,
    /// As the socket and TLS take it: an IPv6 address without its brackets.
    pub(crate) host: String,
    pub(crate) port: u16,
}

struct Idle {
    origin: Origin,
    conn: ConnId,
    since: Duration,
}

pub(crate) struct Pool {
    /// Idle longest first: each is put at the back, at the clock's time, which never goes back.
    idle: VecDeque<Idle>,
    most: usize,
    keep: Duration,
}

impl Pool {
    pub(crate) fn new(most: usize, keep: Duration) -> Pool {
        // alloc: the client's idle list, made once and never grown past `most`.
        Pool { idle: VecDeque::with_capacity(most), most, keep }
    }

    /// The connection to `origin` idle the shortest, taken out.
    pub(crate) fn take(&mut self, origin: &Origin) -> Option<ConnId> {
        let at = self.idle.iter().rposition(|it| it.origin == *origin)?;
        self.idle.remove(at).map(|it| it.conn)
    }

    /// Keeps `conn`, idle from `now`: the connection that goes for it (the one idle longest,
    /// when the pool is full; `conn` itself, when it keeps none), for the client to close.
    pub(crate) fn put(&mut self, origin: Origin, conn: ConnId, now: Duration) -> Option<ConnId> {
        if self.most == 0 {
            return Some(conn);
        }
        let gone = if self.idle.len() >= self.most {
            self.idle.pop_front().map(|it| it.conn)
        } else {
            None
        };
        self.idle.push_back(Idle { origin, conn, since: now });
        gone
    }

    /// Forgets `conn`, which closed while idle.
    pub(crate) fn remove(&mut self, conn: ConnId) {
        self.idle.retain(|it| it.conn != conn);
    }

    /// The connections idle for `keep` or longer at `now`, taken out, idle longest first.
    pub(crate) fn expire(&mut self, now: Duration, gone: &mut Vec<ConnId>) {
        while let Some(first) = self.idle.front() {
            if now.saturating_sub(first.since) < self.keep {
                break;
            }
            gone.extend(self.idle.pop_front().map(|it| it.conn));
        }
    }

    /// When the connection idle longest expires.
    pub(crate) fn next_expiry(&self) -> Option<Duration> {
        self.idle.front().map(|it| it.since.saturating_add(self.keep))
    }
}
