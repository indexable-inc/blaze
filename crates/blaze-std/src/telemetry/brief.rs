//! [`Brief`]: the projection of a `Used` a span row keeps (20 numbers, 160 bytes, against a
//! `Used`'s 1.5 KB), each a monotone counter so a stretch is the difference of two.

use blaze_meter::{Call, Cells, Count, Meter, Reading, System, Tally, Waited};

/// One number of a [`Brief`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Field {
    Instructions,
    Cycles,
    UserNs,
    SystemNs,
    Faults,
    Allocs,
    HeapBytes,
    Frees,
    FreedBytes,
    FrameBytes,
    Mapped,
    StoreGets,
    StoreRead,
    StorePuts,
    StorePutBytes,
    Hashed,
    Calls,
    CallNs,
    Waits,
    WaitNs,
}

/// How many [`Field`]s.
pub const FIELDS: usize = 20;

impl Field {
    /// Every field, in the printed and encoded order.
    pub const ALL: [Field; FIELDS] = [
        Field::Instructions,
        Field::Cycles,
        Field::UserNs,
        Field::SystemNs,
        Field::Faults,
        Field::Allocs,
        Field::HeapBytes,
        Field::Frees,
        Field::FreedBytes,
        Field::FrameBytes,
        Field::Mapped,
        Field::StoreGets,
        Field::StoreRead,
        Field::StorePuts,
        Field::StorePutBytes,
        Field::Hashed,
        Field::Calls,
        Field::CallNs,
        Field::Waits,
        Field::WaitNs,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Field::Instructions => "instr",
            Field::Cycles => "cycles",
            Field::UserNs => "user",
            Field::SystemNs => "sys",
            Field::Faults => "faults",
            Field::Allocs => "allocs",
            Field::HeapBytes => "heap",
            Field::Frees => "frees",
            Field::FreedBytes => "freed",
            Field::FrameBytes => "frames",
            Field::Mapped => "mapped",
            Field::StoreGets => "store gets",
            Field::StoreRead => "store read",
            Field::StorePuts => "puts",
            Field::StorePutBytes => "put",
            Field::Hashed => "hashed",
            Field::Calls => "calls",
            Field::CallNs => "in calls",
            Field::Waits => "waits",
            Field::WaitNs => "waited",
        }
    }

    /// Whether the field is a time in ns, a size in bytes, or a count (how it is printed).
    pub const fn unit(self) -> Unit {
        match self {
            Field::UserNs | Field::SystemNs | Field::CallNs | Field::WaitNs => Unit::Ns,
            Field::HeapBytes | Field::FreedBytes | Field::FrameBytes | Field::Mapped | Field::StoreRead | Field::StorePutBytes | Field::Hashed => {
                Unit::Bytes
            }
            _ => Unit::Count,
        }
    }
}

/// How a field is printed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unit {
    Count,
    Bytes,
    Ns,
}

/// The numbers a span keeps, by [`Field`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Brief {
    pub v: [u64; FIELDS],
}

fn got(reading: Reading) -> u64 {
    match reading.got() {
        Some(n) => n,
        None => 0,
    }
}

impl Brief {
    pub const ZERO: Brief = Brief { v: [0; FIELDS] };

    pub const fn get(&self, field: Field) -> u64 {
        self.v[field as usize]
    }

    /// The projection of `cells` and `waited`, and of `system` where given (a reading the
    /// system does not give is 0).
    pub fn of(cells: &Cells, waited: &Waited, system: Option<&System>) -> Brief {
        let mut v = [0u64; FIELDS];
        if let Some(system) = system {
            v[Field::Instructions as usize] = got(system.instructions);
            v[Field::Cycles as usize] = got(system.cycles);
            v[Field::UserNs as usize] = got(system.user_ns);
            v[Field::SystemNs as usize] = got(system.system_ns);
            v[Field::Faults as usize] = got(system.faults());
        }
        let count = |c: Count| cells.counts[c as usize];
        v[Field::Allocs as usize] = cells.sizes.iter().fold(0u64, |sum, n| sum.wrapping_add(*n));
        v[Field::HeapBytes as usize] = count(Count::HeapBytes);
        v[Field::Frees as usize] = count(Count::HeapFrees);
        v[Field::FreedBytes as usize] = count(Count::HeapFreedBytes);
        v[Field::FrameBytes as usize] = cells.frame_bytes.iter().fold(0u64, |sum, n| sum.wrapping_add(*n));
        v[Field::Mapped as usize] = count(Count::Mapped);
        v[Field::StoreGets as usize] = count(Count::StoreGets);
        v[Field::StoreRead as usize] = count(Count::StoreRead);
        v[Field::StorePuts as usize] = count(Count::StorePuts);
        v[Field::StorePutBytes as usize] = count(Count::StorePutBytes);
        v[Field::Hashed as usize] = count(Count::Hashed);
        let mut calls = 0u64;
        let mut call_ns = 0u64;
        for call in Call::ALL {
            calls = calls.wrapping_add(cells.calls[call as usize]);
            call_ns = call_ns.wrapping_add(cells.call_ns[call as usize]);
        }
        v[Field::Calls as usize] = calls;
        v[Field::CallNs as usize] = call_ns;
        let (waits, wait_ns) = waited.total();
        v[Field::Waits as usize] = waits;
        v[Field::WaitNs as usize] = wait_ns;
        Brief { v }
    }

    /// The worker's `tally` alone: what a simulation feeds, where the thread's own cells
    /// count the harness's allocations too.
    pub fn of_tally(tally: &Tally) -> Brief {
        Brief::of(tally.cells(), tally.waits(), None)
    }

    /// `Spans`' snapshot: the thread's cells and the worker's `tally`, no system call.
    pub fn now(tally: &Tally) -> Brief {
        Brief::of(&blaze_meter::cells().plus(tally.cells()), tally.waits(), None)
    }

    /// `Detail`'s snapshot: [`Meter::now_with`], the system read too.
    pub fn now_detailed(tally: &Tally) -> Brief {
        let meter = Meter::now_with(tally);
        Brief::of(&meter.cells, &meter.waited, Some(&meter.system))
    }

    /// What was added from `before` to `self`.
    pub fn since(&self, before: &Brief) -> Brief {
        let mut v = [0u64; FIELDS];
        for (at, out) in v.iter_mut().enumerate() {
            *out = self.v[at].wrapping_sub(before.v[at]);
        }
        Brief { v }
    }

    pub fn plus(&mut self, other: &Brief) {
        for (mine, theirs) in self.v.iter_mut().zip(other.v.iter()) {
            *mine = mine.wrapping_add(*theirs);
        }
    }

    pub fn most(&mut self, other: &Brief) {
        for (mine, theirs) in self.v.iter_mut().zip(other.v.iter()) {
            if *theirs > *mine {
                *mine = *theirs;
            }
        }
    }
}
