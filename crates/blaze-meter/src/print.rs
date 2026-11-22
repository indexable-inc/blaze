//! The `used:` line: one compact line under each timed row of the profile, only the counters
//! that are not zero, in a fixed order, with units; an unavailable reading is said as such.
//!
//! `used: 1.2 G instr, 1.4 G cycles, 0.62 s user 0.08 s sys, 412 allocs 9.8 MB (most under
//! 2.1 MB), 380 frees 9.1 MB, frames 3.0 MB (12 resets), 3,104 faults, mapped 4.2 MB, 96,285
//! store gets 1.0 GB (1,204 keys), 2 puts 18 kB, hashed 1.1 MB (310 digests), 17 stat 0.4 ms,
//! 2 mmap 0.1 ms, 0 waits`
//!
//! Waits close the line even at zero: "0 waits" says no stretch of it was blocked.

use core::fmt;

use crate::{Call, Count, Field, Group, Overflow, Reading, Unpaired, Used};

/// A cell by name: `sizes[5]`, `calls[stat]`, `counts[3]`.
impl fmt::Display for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let group = match self.group {
            Group::Counts => "counts",
            Group::Sizes => "sizes",
            Group::FrameBytes => "frame_bytes",
            Group::FrameResets => "frame_resets",
            Group::Calls => "calls",
            Group::CallNs => "call_ns",
        };
        match self.group {
            Group::Calls | Group::CallNs => match Call::ALL.iter().find(|call| **call as usize == self.index) {
                Some(call) => write!(f, "{group}[{}]", call.name()),
                None => write!(f, "{group}[{}]", self.index),
            },
            _ => write!(f, "{group}[{}]", self.index),
        }
    }
}

impl fmt::Display for Unpaired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unpaired::OtherThread => f.write_str("two threads' snapshots"),
            Unpaired::OtherTally => f.write_str("snapshots of two tallies (or one with a tally, one without)"),
            Unpaired::AcrossStop => f.write_str("snapshots on either side of the tests' meter switch"),
            Unpaired::Backwards { field, before, now } => write!(f, "{field} went down from {before} to {now}: not one owner's counters"),
        }
    }
}

impl fmt::Display for Overflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} passed u64 at {}", self.sum, self.at)
    }
}

/// A count with thousands separated: `96,285`.
pub struct Grouped(pub u64);

impl fmt::Display for Grouped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.0;
        let mut groups = [0u16; 7];
        let mut len = 0;
        let mut rest = n;
        loop {
            groups[len] = (rest % 1000) as u16;
            len += 1;
            rest /= 1000;
            if rest == 0 {
                break;
            }
        }
        write!(f, "{}", groups[len - 1])?;
        for group in groups[..len - 1].iter().rev() {
            write!(f, ",{group:03}")?;
        }
        Ok(())
    }
}

/// A large count in a decimal unit: `1.2 G`, `48 M`, `3.1 k`; below a thousand as is.
pub struct Large(pub u64);

impl fmt::Display for Large {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        scaled(f, self.0, &[(1_000_000_000_000, " T"), (1_000_000_000, " G"), (1_000_000, " M"), (1_000, " k")], "")
    }
}

/// Bytes in a decimal unit: `9.8 MB`, `1.0 GB`, `512 B`.
pub struct Bytes(pub u64);

impl fmt::Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        scaled(f, self.0, &[(1_000_000_000_000, " TB"), (1_000_000_000, " GB"), (1_000_000, " MB"), (1_000, " kB")], " B")
    }
}

/// Nanoseconds as seconds, milliseconds or microseconds: `0.62 s`, `14 ms`, `310 us`.
pub struct Ns(pub u64);

impl fmt::Display for Ns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        scaled(f, self.0, &[(1_000_000_000, " s"), (1_000_000, " ms"), (1_000, " us")], " ns")
    }
}

/// `n` in the first unit it reaches, with two significant digits below 10 and none above.
fn scaled(f: &mut fmt::Formatter<'_>, n: u64, units: &[(u64, &str)], below: &str) -> fmt::Result {
    for &(unit, name) in units {
        if n >= unit {
            let tenths = n / (unit / 10);
            return match tenths < 100 {
                true => write!(f, "{}.{}{name}", tenths / 10, tenths % 10),
                false => write!(f, "{}{name}", n / unit),
            };
        }
    }
    write!(f, "{n}{below}")
}

/// One reading: its value in `show`'s form, or "<label> unavailable (<why>)".
fn reading(f: &mut fmt::Formatter<'_>, first: &mut bool, read: Reading, label: &str, show: fn(&mut fmt::Formatter<'_>, u64, &str) -> fmt::Result) -> fmt::Result {
    match read {
        Reading::Got(0) => Ok(()),
        Reading::Got(n) => {
            sep(f, first)?;
            show(f, n, label)
        }
        Reading::Unavailable(why) => {
            sep(f, first)?;
            write!(f, "{label} unavailable ({})", why.says())
        }
    }
}

fn sep(f: &mut fmt::Formatter<'_>, first: &mut bool) -> fmt::Result {
    match core::mem::replace(first, false) {
        true => Ok(()),
        false => f.write_str(", "),
    }
}

impl fmt::Display for Used {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("used: ")?;
        let first = &mut true;
        if let Some(field) = self.overflowed {
            sep(f, first)?;
            write!(f, "OVERFLOWED {field} (a sum past u64, saturated)")?;
        }
        let s = self.system;
        reading(f, first, s.instructions, "instr", |f, n, label| write!(f, "{} {label}", Large(n)))?;
        reading(f, first, s.cycles, "cycles", |f, n, label| write!(f, "{} {label}", Large(n)))?;
        reading(f, first, s.user_ns, "user", |f, n, label| write!(f, "{} {label}", Ns(n)))?;
        reading(f, first, s.system_ns, "sys", |f, n, label| write!(f, "{} {label}", Ns(n)))?;
        match self.allocations() {
            Ok(0) => {}
            Ok(allocs) => {
                sep(f, first)?;
                write!(f, "{} allocs {}", Grouped(allocs), Bytes(self.count(Count::HeapBytes)))?;
                match self.largest_under() {
                    Some(u64::MAX) => write!(f, " (most over {})", Bytes(1u64 << (crate::SIZES - 1)))?,
                    Some(top) => write!(f, " (most under {})", Bytes(top))?,
                    None => {}
                }
            }
            Err(overflow) => {
                sep(f, first)?;
                write!(f, "OVERFLOWED {overflow}")?;
            }
        }
        if self.count(Count::HeapFrees) > 0 {
            sep(f, first)?;
            write!(f, "{} frees {}", Grouped(self.count(Count::HeapFrees)), Bytes(self.count(Count::HeapFreedBytes)))?;
        }
        match (self.frame_bytes(), self.frame_resets()) {
            (Err(overflow), _) | (_, Err(overflow)) => {
                sep(f, first)?;
                write!(f, "OVERFLOWED {overflow}")?;
            }
            (Ok(_), Ok(0)) => {}
            (Ok(bytes), Ok(resets)) => {
                sep(f, first)?;
                write!(f, "frames {} ({} resets)", Bytes(bytes), Grouped(resets))?;
            }
        }
        reading(f, first, s.faults(), "faults", |f, n, label| write!(f, "{} {label}", Grouped(n)))?;
        if self.count(Count::Mapped) > 0 {
            sep(f, first)?;
            write!(f, "mapped {}", Bytes(self.count(Count::Mapped)))?;
        }
        if self.count(Count::StoreGets) > 0 {
            sep(f, first)?;
            write!(f, "{} store gets {} ({} keys)", Grouped(self.count(Count::StoreGets)), Bytes(self.count(Count::StoreRead)), Grouped(self.count(Count::StoreKeys)))?;
        }
        if self.count(Count::StoreCopied) > 0 {
            sep(f, first)?;
            write!(f, "copied from the store {}", Bytes(self.count(Count::StoreCopied)))?;
        }
        if self.count(Count::StorePuts) > 0 {
            sep(f, first)?;
            write!(f, "{} puts {}", Grouped(self.count(Count::StorePuts)), Bytes(self.count(Count::StorePutBytes)))?;
        }
        if self.count(Count::Hashed) > 0 || self.count(Count::Digests) > 0 {
            sep(f, first)?;
            write!(f, "hashed {} ({} digests)", Bytes(self.count(Count::Hashed)), Grouped(self.count(Count::Digests)))?;
        }
        for call in Call::ALL {
            let (count, ns) = self.calls(call);
            if count > 0 {
                sep(f, first)?;
                write!(f, "{} {}", Grouped(count), call.name())?;
                if ns > 0 {
                    write!(f, " {}", Ns(ns))?;
                }
            }
        }
        // The stretch's waits, by what they waited on (D171): charged at each wait's way out.
        if self.waited.total().0 > 0 {
            sep(f, first)?;
            write!(f, "{}", self.waited)?;
        }
        // The lock that made the stretch wait longest, and who held it: `waited 12 ms on lock
        // `store.head`, held by `merge``.
        if let Some(row) = self.locks.by_wait()[0] {
            sep(f, first)?;
            write!(f, "waited {} on lock `{}`", Ns(row.ns), row.lock)?;
            if let Some(top) = row.top {
                write!(f, ", held by `{}`", Doing(top.doing))?;
            }
        }
        sep(f, first)?;
        match self.waits() {
            Ok((waits, ns)) => {
                write!(f, "{} waits", Grouped(waits))?;
                if ns > 0 {
                    write!(f, " {}", Ns(ns))?;
                }
            }
            Err(overflow) => write!(f, "OVERFLOWED {overflow}")?,
        }
        Ok(())
    }
}

/// A holder's task name, `a thread with no task` for none.
struct Doing(&'static str);

impl fmt::Display for Doing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            "" => f.write_str("a thread with no task"),
            name => f.write_str(name),
        }
    }
}

/// How many locks the `locks:` section lists.
pub const REPORT_LOCKS: usize = 10;

/// The profile's `locks:` section (docs/v2/plan/design/lock-policy.md, "A lock earns its
/// place"): of a command's whole [`Used`] (every worker's stretches summed) and its worker
/// time.
///
/// ```text
/// locks: 3 contended, 14 waits 41.2 ms = 0.4% of 9.8 s worker time
///   store.head     9 waits 30.1 ms, longest 20.0 ms; most by `merge` 25.0 ms (hold up to 18.0 ms)
///   sched.ready    4 waits 10.0 ms, longest 6.1 ms; 3 writers behind readers; most by `parse` 9.0 ms (hold at least 4.0 ms)
///   other          1 wait 1.1 ms
///   never contended: not counted (counting acquires would cost the uncontended path)
/// ```
///
/// The breakdown reconciles with the meter's own lock waits; a difference is printed as an
/// `UNACCOUNTED:` line, never hidden.
pub struct LocksReport<'a> {
    pub used: &'a Used,
    /// The command's worker time (its workers' busy nanoseconds); `None` where the caller has
    /// no such number.
    pub worker_ns: Option<u64>,
}

impl fmt::Display for LocksReport<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let locks = &self.used.locks;
        let (count, ns) = locks.total();
        write!(f, "locks: {} contended, {} waits {}", locks.named(), Grouped(count), Ns(ns))?;
        if let Some(worker_ns) = self.worker_ns.filter(|ns| *ns > 0) {
            let per_mille = u128::from(ns) * 1000 / u128::from(worker_ns);
            write!(f, " = {}.{}% of {} worker time", per_mille / 10, per_mille % 10, Ns(worker_ns))?;
        }
        for row in locks.by_wait().iter().flatten().take(REPORT_LOCKS) {
            write!(f, "\n  {:<24} {} waits {}, longest {}", row.lock, Grouped(row.contended), Ns(row.ns), Ns(row.longest_ns))?;
            if row.reads_behind_write > 0 {
                write!(f, "; {} readers behind a writer", Grouped(row.reads_behind_write))?;
            }
            if row.writes_behind_read > 0 {
                write!(f, "; {} writers behind readers", Grouped(row.writes_behind_read))?;
            }
            if let Some(top) = row.top {
                let bound = if top.whole { "up to" } else { "at least" };
                write!(f, "; most by `{}` {} (hold {bound} {})", Doing(top.doing), Ns(top.caused_ns), Ns(top.longest_hold_ns))?;
            }
            if row.unseen_holder > 0 {
                write!(f, "; {} with no holder seen", Grouped(row.unseen_holder))?;
            }
        }
        let shown: u64 = locks.by_wait().iter().flatten().take(REPORT_LOCKS).map(|row| row.contended).sum();
        let shown_ns: u64 = locks.by_wait().iter().flatten().take(REPORT_LOCKS).map(|row| row.ns).sum();
        let (rest, rest_ns) = (count.saturating_sub(shown), ns.saturating_sub(shown_ns));
        if rest > 0 {
            write!(f, "\n  {:<24} {} waits {}", "other", Grouped(rest), Ns(rest_ns))?;
        }
        f.write_str("\n  never contended: not counted (counting acquires would cost the uncontended path)")?;
        if !self.used.locks_reconcile() {
            write!(
                f,
                "\n  UNACCOUNTED: the meter's lock waits ({} waits {}) are not the table's ({} waits {})",
                Grouped(self.used.count(Count::LockWaits)),
                Ns(self.used.count(Count::LockWaitNs)),
                Grouped(count),
                Ns(ns)
            )?;
        }
        Ok(())
    }
}
