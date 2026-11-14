//! The in-memory bucket of the simulation: one shared object map, any number of clients on it,
//! each with its own virtual clock, latency, faults and counts. An op takes effect when its
//! answer is delivered, so clients stepped in turn interleave at request grain.
//!
//! Faults, per client, each drawn from the client's seeded generator:
//! - `fail_one_in`: the op does not take effect, answered [`Reply::Failed`];
//! - `ambiguous_one_in`: the op takes effect, answered [`Reply::Failed`] (the answer was lost);
//! - `timeout_one_in`: no answer by the deadline, answered [`Reply::TimedOut`]; it takes effect
//!   or not, by a coin;
//! - `crash_after`: after that many ops took effect, the client is dead: nothing more takes
//!   effect, every answer is [`Reply::Failed`]. A crash at every step is a run per value.

use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

use blaze_rng::SplitMix64;

use crate::bucket::{Asked, Bucket, Cond, Counts, Op, Reply, Ticket};

#[derive(Clone, Debug)]
pub(crate) struct Object {
    pub(crate) bytes: Vec<u8>,
    pub(crate) version: u64,
}

impl Object {
    fn etag(&self) -> String {
        format!("\"v{}\"", self.version)
    }
}

#[derive(Clone, Default, Debug)]
pub(crate) struct Store {
    pub(crate) objects: BTreeMap<String, Object>,
    uploads: BTreeMap<String, (String, BTreeMap<u32, Vec<u8>>)>,
    next_version: u64,
    next_upload: u64,
    /// Requests asked of the bucket by every client.
    pub(crate) requests: u64,
    /// Parts of multipart uploads taken, and uploads finished into an object.
    pub(crate) parts: u64,
    pub(crate) multipart: u64,
}

impl Store {
    fn version(&mut self) -> u64 {
        self.next_version += 1;
        self.next_version
    }

    fn holds(&self, name: &str, cond: &Cond) -> bool {
        match (cond, self.objects.get(name)) {
            (Cond::Any, _) => true,
            (Cond::Absent, found) => found.is_none(),
            (Cond::Matches(etag), Some(it)) => it.etag() == *etag,
            (Cond::Matches(_), None) => false,
        }
    }

    /// `op` taking effect: its answer.
    pub(crate) fn apply(&mut self, op: &Op) -> Reply {
        match op {
            Op::Get { name, range, most } => {
                let Some(it) = self.objects.get(name) else { return Reply::Missing };
                let bytes = match range {
                    None => it.bytes.clone(),
                    Some((at, len)) => match it.bytes.get(*at as usize..(*at + *len) as usize) {
                        Some(part) => part.to_vec(),
                        None => return Reply::Failed { why: format!("{name}: range outside the object") },
                    },
                };
                if bytes.len() as u64 > *most {
                    return Reply::Failed { why: format!("{name}: {} bytes, more than the {most} asked at most", bytes.len()) };
                }
                Reply::Got { bytes, etag: it.etag() }
            }
            Op::Head { name } => self.objects.get(name).map_or(Reply::Missing, |it| Reply::Exists { len: it.bytes.len() as u64 }),
            Op::Put { name, bytes, cond } => {
                if !self.holds(name, cond) {
                    return Reply::Precondition;
                }
                let version = self.version();
                let object = Object { bytes: bytes.clone(), version };
                let etag = object.etag();
                self.objects.insert(name.clone(), object);
                Reply::Done { etag }
            }
            Op::Begin { name } => {
                self.next_upload += 1;
                let upload = format!("upload-{}", self.next_upload);
                self.uploads.insert(upload.clone(), (name.clone(), BTreeMap::new()));
                Reply::Begun { upload }
            }
            Op::Part { name, upload, number, bytes } => match self.uploads.get_mut(upload) {
                Some((for_name, parts)) if for_name == name => {
                    parts.insert(*number, bytes.clone());
                    self.parts += 1;
                    Reply::Done { etag: format!("\"p{number}-{}\"", bytes.len()) }
                }
                _ => Reply::Failed { why: format!("{name}: no such upload") },
            },
            Op::Finish { name, upload, parts, cond } => {
                let Some((for_name, held)) = self.uploads.get(upload) else { return Reply::Failed { why: format!("{name}: no such upload") } };
                if for_name != name || parts.iter().any(|(n, etag)| held.get(n).is_none_or(|it| format!("\"p{n}-{}\"", it.len()) != *etag)) || parts.len() != held.len() {
                    return Reply::Failed { why: format!("{name}: parts not the ones uploaded") };
                }
                if !self.holds(name, cond) {
                    self.uploads.remove(upload);
                    return Reply::Precondition;
                }
                let (_, held) = self.uploads.remove(upload).expect("just seen");
                let bytes = held.into_values().flatten().collect();
                self.multipart += 1;
                let version = self.version();
                let object = Object { bytes, version };
                let etag = object.etag();
                self.objects.insert(name.clone(), object);
                Reply::Done { etag }
            }
        }
    }
}

pub(crate) type Shared = Rc<RefCell<Store>>;

pub(crate) fn shared() -> Shared {
    Rc::new(RefCell::new(Store::default()))
}

/// A copy of the bucket as it is: a run from this point that leaves the original as it was.
pub(crate) fn fork(store: &Shared) -> Shared {
    Rc::new(RefCell::new(store.borrow().clone()))
}

#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct Faults {
    /// Latency of an answer: up to this many ms.
    pub(crate) latency_ms: u64,
    pub(crate) fail_one_in: u64,
    pub(crate) ambiguous_one_in: u64,
    pub(crate) timeout_one_in: u64,
    pub(crate) crash_after: Option<u64>,
}

struct Flying {
    ticket: Ticket,
    op: Op,
    ready_ms: u64,
    deadline_ms: u64,
}

/// One client of the fake bucket.
pub(crate) struct Fake {
    store: Shared,
    rng: SplitMix64,
    faults: Faults,
    now: u64,
    flying: Vec<Flying>,
    applied: u64,
    counts: Counts,
}

impl Fake {
    pub(crate) fn new(store: &Shared, seed: u64, faults: Faults) -> Fake {
        Fake { store: Rc::clone(store), rng: SplitMix64::new(seed), faults, now: 0, flying: Vec::new(), applied: 0, counts: Counts::default() }
    }

    pub(crate) fn dead(&self) -> bool {
        self.faults.crash_after.is_some_and(|it| self.applied >= it)
    }

    fn one_in(&mut self, n: u64) -> bool {
        n > 0 && self.rng.below(n) == 0
    }

    fn deliver(&mut self, it: Flying) -> (Ticket, Reply) {
        if self.dead() {
            return (it.ticket, Reply::Failed { why: "the client crashed".to_owned() });
        }
        let late = it.ready_ms > it.deadline_ms;
        if late {
            if self.rng.below(2) == 0 {
                self.applied += 1;
                drop(self.store.borrow_mut().apply(&it.op));
            }
            return (it.ticket, Reply::TimedOut);
        }
        if self.one_in(self.faults.fail_one_in) {
            return (it.ticket, Reply::Failed { why: format!("{}: injected failure", it.op.name()) });
        }
        self.applied += 1;
        let reply = self.store.borrow_mut().apply(&it.op);
        if self.one_in(self.faults.ambiguous_one_in) {
            return (it.ticket, Reply::Failed { why: format!("{}: injected failure after it took effect", it.op.name()) });
        }
        if let Reply::Got { bytes, .. } = &reply {
            self.counts.received += bytes.len() as u64;
        }
        (it.ticket, reply)
    }

    /// Delivers every answer ready by now, the earliest first; `None` with nothing in flight.
    pub(crate) fn deliver_next(&mut self) -> Option<Vec<(Ticket, Reply)>> {
        let first = self.flying.iter().map(|it| it.ready_ms.min(it.deadline_ms + 1)).min()?;
        self.now = self.now.max(first);
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.flying.len() {
            if self.flying[i].ready_ms.min(self.flying[i].deadline_ms + 1) <= self.now {
                let it = self.flying.remove(i);
                out.push(self.deliver(it));
            } else {
                i += 1;
            }
        }
        Some(out)
    }
}

impl Bucket for Fake {
    fn now_ms(&mut self) -> u64 {
        self.now
    }

    fn send(&mut self, asked: Vec<Asked>) {
        for Asked { ticket, op, deadline_ms } in asked {
            assert!(self.flying.iter().all(|it| it.ticket != ticket), "ticket {} asked twice while in flight", ticket.0);
            self.counts.requests += 1;
            self.counts.sent += op.sends();
            self.store.borrow_mut().requests += 1;
            let latency = if self.faults.latency_ms == 0 { 1 } else { 1 + self.rng.below(self.faults.latency_ms) };
            let ready_ms = if self.one_in(self.faults.timeout_one_in) { deadline_ms + 1 } else { self.now + latency };
            self.flying.push(Flying { ticket, op, ready_ms, deadline_ms });
        }
    }

    fn in_flight(&self) -> usize {
        self.flying.len()
    }

    fn wait(&mut self) -> Vec<(Ticket, Reply)> {
        self.deliver_next().unwrap_or_default()
    }

    fn counts(&self) -> Counts {
        self.counts
    }
}
