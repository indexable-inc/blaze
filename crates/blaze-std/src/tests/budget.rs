//! A budget is never exceeded, and is spent exactly: a `Net` with a byte limit and a `Dir`
//! with a quota each let through what fits and refuse, with a typed `Over`, what does not.
//!
//! Each oracle looks past bstd's own count. For the limit it is what the peer outside the
//! simulation was sent, and what it sent that was received; for the quota, the lengths of the
//! files on the simulated disk. A refused send or write must have moved nothing.
//!
//! Plant: [`Plant::BudgetCheckedAfter`], a limit looked at once the bytes are gone.

use std::net::SocketAddr;

use blaze_sim::{Config, Tape};

use super::{check, control, path, root, world};
use crate::{
    Cx, Plant,
    fs::{Content, Dir, FsKind, ReadWrite, Unsynced},
    io::Io,
    net::{Addr, Conn, HostPort, Listener, NetKind},
};

#[derive(Clone, Copy, Debug)]
enum Flow {
    /// The limited side sends this many bytes.
    Send(usize),
    /// The peer sends this many, then the limited side receives at most the second.
    Recv(usize, usize),
}

#[derive(Debug)]
struct NetPlan {
    seed: u64,
    limit: u64,
    /// A second limit set on the limited `Net`, inside the first: the lower of the two rules.
    inner: Option<u64>,
    flows: Vec<Flow>,
}

fn net_plan(tape: &Tape) -> NetPlan {
    let seed = tape.next_u64("sim seed");
    let limit = tape.int_in(0..=200u64, "limit");
    let inner = tape.coin("inner limit").then(|| tape.int_in(0..=300u64, "inner"));
    let flows = (0..tape.int_in(1..=24u64, "flows"))
        .map(|_| match tape.coin("send") {
            true => Flow::Send(tape.int_in(0..=48u64, "send len") as usize),
            false => Flow::Recv(tape.int_in(1..=48u64, "peer sends") as usize, tape.int_in(1..=48u64, "recv most") as usize),
        })
        .collect();
    NetPlan { seed, limit, inner, flows }
}

const AT: &str = "127.0.0.1:9000";

/// The plan's flows on `conn`, whose `Net` has `limit` bytes; `peer` is the other end, which
/// the test plays from outside the simulation.
fn flows(plan: &NetPlan, io: &Io, cx: &Cx, conn: &Conn, peer: blaze_core::Conn, limit: u64) -> Result<(), String> {
    // What the model says is used of the limit; what the peer was sent; what it sent and is
    // not received yet.
    let (mut used, mut delivered, mut unread): (u64, u64, usize) = (0, 0, 0);
    let mut received: u64 = 0;
    for (at, &flow) in plan.flows.iter().enumerate() {
        match flow {
            Flow::Send(len) => {
                let bytes = vec![at as u8; len];
                let fits = used + len as u64 <= limit;
                let sent = conn.send_all(cx, &bytes);
                let got = io.with_sim(|sim| sim.peer_received(peer)).ok_or("harness: the world is not simulated")?;
                delivered += got.len() as u64;
                match (sent, fits) {
                    (Ok(()), true) if got == bytes => used += len as u64,
                    (Err(error), false) if matches!(error.kind, NetKind::OverLimit(_)) => {
                        if !got.is_empty() {
                            return Err(format!(
                                "flow {at}: a send of {len} was refused by the limit of {limit} ({used} used), and {} of its bytes reached the peer",
                                got.len()
                            ));
                        }
                    }
                    (sent, _) => {
                        return Err(format!(
                            "flow {at}: a send of {len} with {used} of {limit} used gave {sent:?}, and the peer got {} bytes",
                            got.len()
                        ));
                    }
                }
            }
            Flow::Recv(sends, most) => {
                io.with_sim(|sim| sim.peer_send(peer, &vec![at as u8; sends])).ok_or("harness: the world is not simulated")?;
                unread += sends;
                let left = limit - used;
                let mut into = cx.bytes();
                match conn.recv(cx, most, &mut into) {
                    Ok(read) if left > 0 && read >= 1 && read as u64 <= left && read <= most.min(unread) => {
                        used += read as u64;
                        received += read as u64;
                        unread -= read;
                    }
                    Err(error) if left == 0 && matches!(error.kind, NetKind::OverLimit(_)) => {}
                    got => {
                        return Err(format!(
                            "flow {at}: a receive of at most {most} with {unread} unread and {left} of the limit left gave {got:?}"
                        ));
                    }
                }
            }
        }
        if delivered + received > limit {
            return Err(format!(
                "flow {at}: {delivered} bytes sent and {received} received through a Net limited to {limit}"
            ));
        }
    }
    Ok(())
}

/// The flows on the connection `listener` accepts from `peer`.
fn served(plan: &NetPlan, io: &Io, cx: &Cx, listener: &mut Listener, peer: blaze_core::Conn, limit: u64) -> Result<(), String> {
    // The connection is linear: the flows borrow it, then it is finished whatever they found.
    let conn = match listener.accept(cx) {
        Ok(conn) => conn,
        Err(error) => return Err(format!("harness: {error}")),
    };
    let flowed = flows(plan, io, cx, &conn, peer, limit);
    let finished = conn.finish(cx);
    match (flowed, finished) {
        (Err(why), _) => Err(why),
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) => Err(format!("harness: finish: {error}")),
    }
}

fn net_run(plan: &NetPlan, plant: Plant) -> Result<(), String> {
    let io = world(plan.seed, plant);
    let cx = root(&io);
    let at = Addr::Tcp(HostPort::parse(AT).expect("a test's address"));
    let net = io.need_net().map_err(|e| format!("harness: {e}"))?.limit(plan.limit);
    let (net, limit) = match plan.inner {
        Some(inner) => (net.limit(inner), plan.limit.min(inner)),
        None => (net, plan.limit),
    };
    if net.limit_left() != Some(limit) {
        return Err(format!("a Net limited to {limit} says it has {:?} left", net.limit_left()));
    }
    // The listener is linear: closed whatever the flows found.
    let mut listener = match net.listen_only(&at).listen(&cx, &at) {
        Ok(listener) => listener,
        Err(error) => return Err(format!("harness: {error}")),
    };
    let socket: SocketAddr = AT.parse().expect("a test's address");
    let dialled = io.with_sim(|sim| sim.dial(&blaze_core::Address::Tcp(socket)));
    let flowed = match dialled {
        Some(Ok(peer)) => served(plan, &io, &cx, &mut listener, peer, limit),
        Some(Err(error)) => Err(format!("harness: dial: {error}")),
        None => Err("harness: the world is not simulated".to_owned()),
    };
    let closed = listener.close(&cx);
    match (flowed, closed) {
        (Err(why), _) => Err(why),
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) => Err(format!("harness: close: {error}")),
    }
}

/// The invariant: a `Net` with a byte limit never passes it and spends it exactly. What the
/// peer was sent and what was received from it, counted outside bstd, fit the limit; a send
/// that does not fit is a typed `Over` and moves nothing.
#[test]
fn a_net_never_passes_its_byte_limit() {
    blaze_sim::check(Config::cases(150), |tape| check(net_plan(tape), net_run));
}

#[test]
fn the_oracle_catches_a_limit_checked_after_the_send() {
    control(Plant::BudgetCheckedAfter, net_plan, net_run);
}

const FILES: [&str; 3] = ["a", "b", "sub/c"];

#[derive(Clone, Copy, Debug)]
enum Change {
    /// The file made to hold this many bytes, all at once.
    Write(usize, usize),
    /// This many bytes appended to the file, if it is there.
    Append(usize, usize),
}

#[derive(Debug)]
struct QuotaPlan {
    seed: u64,
    quota: u64,
    changes: Vec<Change>,
}

fn quota_plan(tape: &Tape) -> QuotaPlan {
    let seed = tape.next_u64("sim seed");
    let quota = tape.int_in(0..=200u64, "quota");
    let changes = (0..tape.int_in(1..=24u64, "changes"))
        .map(|_| match tape.coin("write") {
            true => Change::Write(tape.int_in(0..=2u64, "file") as usize, tape.int_in(0..=48u64, "len") as usize),
            false => Change::Append(tape.int_in(0..=2u64, "file") as usize, tape.int_in(0..=48u64, "len") as usize),
        })
        .collect();
    QuotaPlan { seed, quota, changes }
}

/// `len` bytes appended to the file at `name` through `limited`: `None` when the file is not
/// there, else how the append went.
fn append(cx: &Cx, limited: &Dir<ReadWrite>, name: &str, len: usize) -> Result<Option<Result<(), crate::fs::FsError>>, String> {
    // The file is linear: closed whatever its append did.
    let file = match limited.open(cx, &path(name)) {
        Ok(file) => file,
        Err(error) if error.kind == FsKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("harness: {error}")),
    };
    let appended = file.append_unsynced(cx, &vec![7; len], Unsynced::No);
    // Nothing here is read after a crash: closed unsynced, which cannot fail or wait.
    match file.close_unsynced(Unsynced::No) {
        Ok(_) => Ok(Some(appended)),
        Err(error) => Err(format!("harness: close: {error}")),
    }
}

fn quota_run(plan: &QuotaPlan, plant: Plant) -> Result<(), String> {
    let io = world(plan.seed, plant);
    let cx = root(&io);
    let fs = io.need_fs().map_err(|e| format!("harness: {e}"))?;
    let dir = fs.open_dir(&cx, &path("data")).map_err(|e| format!("harness: {e}"))?;
    let limited = dir.quota(plan.quota);
    let mut used: u64 = 0;
    for (at, &change) in plan.changes.iter().enumerate() {
        let (file, len, changed) = match change {
            Change::Write(file, len) => (file, len, Some(limited.publish_unsynced(&cx, &path(FILES[file]), Content::bytes(&vec![3; len]), Unsynced::No).map(|_| ()))),
            Change::Append(file, len) => (file, len, append(&cx, &limited, FILES[file], len)?),
        };
        let fits = used + len as u64 <= plan.quota;
        match (changed, fits) {
            // An append to a file that is not there writes nothing and takes nothing.
            (None, _) => {}
            (Some(Ok(())), true) => used += len as u64,
            (Some(Err(error)), false) if matches!(error.kind, FsKind::OverQuota(_)) => {}
            (Some(changed), _) => {
                return Err(format!(
                    "change {at}: {len} bytes to {} with {used} of the quota of {} used gave {changed:?}",
                    FILES[file], plan.quota
                ));
            }
        }
        if limited.quota_left() != Some(plan.quota - used) {
            return Err(format!(
                "change {at}: the directory says {:?} of its quota is left, and the model {}",
                limited.quota_left(),
                plan.quota - used
            ));
        }
        // What is on the disk, read through the directory that has no quota.
        let mut held: u64 = 0;
        for name in FILES {
            let meta = dir.stat(&cx, &path(name)).map_err(|e| format!("harness: {e}"))?;
            held += meta.map_or(0, |meta| meta.len);
        }
        if held > plan.quota {
            return Err(format!(
                "change {at}: the files written through a directory with a quota of {} hold {held} bytes",
                plan.quota
            ));
        }
    }
    Ok(())
}

/// The invariant: a `Dir` with a quota never passes it and spends it exactly. The lengths of
/// the files on the simulated disk fit the quota; a write that does not fit is a typed `Over`
/// and writes nothing.
#[test]
fn a_directory_never_passes_its_quota() {
    blaze_sim::check(Config::cases(150), |tape| check(quota_plan(tape), quota_run));
}

#[test]
fn the_oracle_catches_a_quota_checked_after_the_write() {
    control(Plant::BudgetCheckedAfter, quota_plan, quota_run);
}
