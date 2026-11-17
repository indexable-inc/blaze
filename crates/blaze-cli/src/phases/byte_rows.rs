//! Where a command's bytes are (rule 16: anything large is a P0 until the profile names it):
//! the records the command made, by kind, with each kind's largest named; a `CrateScope`
//! record's sections, summed and for the largest; and the frames its tasks ran in, by kind. Rows
//! of `.blaze/v2/profile.txt`, one line each, kept in fixed tables: nothing is allocated to
//! count, and a record's name is written only when it is its kind's largest so far.
//!
//! The rows, to grep:
//!
//! - `record bytes: <Kind> (records, bytes, largest: <name>) <n> <sum> <largest>`
//! - `scope section bytes: <section> (summed over the command, in the largest: <crate>) <sum> <in the largest>`
//! - `scope section bytes: CrateScope records not of the sections' form <n>`
//! - `task frame bytes: <Kind> (frames, summed, most) <n> <sum> <most>`

use core::fmt;

use blaze_drive::Kind;
use blaze_drive::key::KINDS;

use crate::names::sections::{SECTIONS, Section};

/// The most bytes of a largest record's name kept: a crate's name, a file's path, a task's name.
const LABEL: usize = 96;

/// A name kept without allocating: the first [`LABEL`] bytes written, cut at a char boundary.
#[derive(Clone, Copy)]
pub struct Label {
    bytes: [u8; LABEL],
    len: usize,
}

impl Label {
    const EMPTY: Label = Label { bytes: [0; LABEL], len: 0 };

    /// `args` written into a label, cut at [`LABEL`] bytes.
    pub(crate) fn of(args: fmt::Arguments<'_>) -> Label {
        let mut label = Label::EMPTY;
        // `write_str` below never fails; an error is a `Display` impl's own, and the label then
        // holds what it wrote before it: a name cut short is still the record's name.
        let _cut_short_is_still_a_name = fmt::write(&mut label, args);
        label
    }

    pub(crate) fn as_str(&self) -> &str {
        // Only whole chars are copied in (`write_str`): the bytes are UTF-8.
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or_default()
    }
}

impl fmt::Write for Label {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut take = s.len().min(LABEL - self.len);
        while !s.is_char_boundary(take) {
            take -= 1;
        }
        self.bytes[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

/// One kind's records made this command.
#[derive(Clone, Copy)]
struct KindRecords {
    records: u64,
    bytes: u64,
    largest: u64,
    of: Label,
}

/// The `CrateScope` records' sections, in [`Section::ALL`]'s order: summed over the command,
/// and the largest record's (by its whole payload) with its crate.
#[derive(Clone, Copy)]
struct ScopeSections {
    records: u64,
    summed: [u64; SECTIONS],
    largest: u64,
    parts: [u64; SECTIONS],
    of: Label,
    /// A `CrateScope` payload, not empty, that `sections::sections` refused: counted, never
    /// dropped from the rows quietly.
    unread: u64,
}

/// The records a command made, by kind: each one handed to the machine, which files it in the
/// store (work.rs `answer_batch`). A filed failure is a record too, with no payload.
#[derive(Clone, Copy)]
pub struct Records {
    kinds: [KindRecords; KINDS],
    scope: ScopeSections,
}

impl Records {
    pub(crate) const fn new() -> Records {
        Records {
            kinds: [KindRecords { records: 0, bytes: 0, largest: 0, of: Label::EMPTY }; KINDS],
            scope: ScopeSections { records: 0, summed: [0; SECTIONS], largest: 0, parts: [0; SECTIONS], of: Label::EMPTY, unread: 0 },
        }
    }

    /// A record of `kind` with `payload` bytes was made; `name` names its task, asked only when
    /// the record is its kind's largest so far.
    pub(crate) fn made(&mut self, kind: Kind, payload: usize, name: impl FnOnce() -> Label) {
        let row = &mut self.kinds[kind.index()];
        row.records += 1;
        row.bytes += payload as u64;
        if row.records == 1 || payload as u64 > row.largest {
            row.largest = payload as u64;
            row.of = name();
        }
    }

    /// A `CrateScope` record of `payload` bytes with these section lengths (in
    /// [`Section::ALL`]'s order); `name` names its crate, asked only when it is the largest so far.
    pub(crate) fn scope(&mut self, parts: [usize; SECTIONS], payload: usize, name: impl FnOnce() -> Label) {
        let s = &mut self.scope;
        s.records += 1;
        for (sum, part) in s.summed.iter_mut().zip(parts) {
            *sum += part as u64;
        }
        if s.records == 1 || payload as u64 > s.largest {
            s.largest = payload as u64;
            s.parts = parts.map(|part| part as u64);
            s.of = name();
        }
    }

    /// A `CrateScope` payload that is not of the sections' form.
    pub(crate) fn scope_unread(&mut self) {
        self.scope.unread += 1;
    }
}

impl fmt::Display for Records {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for kind in Kind::ALL {
            let row = &self.kinds[kind.index()];
            if row.records == 0 {
                continue;
            }
            writeln!(f, "record bytes: {} (records, bytes, largest: {}) {} {} {}", kind.name(), row.of.as_str(), row.records, row.bytes, row.largest)?;
        }
        let s = &self.scope;
        if s.records > 0 {
            for (i, section) in Section::ALL.into_iter().enumerate() {
                writeln!(f, "scope section bytes: {} (summed over the command, in the largest: {}) {} {}", section.name(), s.of.as_str(), s.summed[i], s.parts[i])?;
            }
        }
        if s.unread > 0 {
            writeln!(f, "scope section bytes: CrateScope records not of the sections' form {}", s.unread)?;
        }
        Ok(())
    }
}

/// One kind's task frames: how many ended, their used bytes at the end summed, the most.
#[derive(Clone, Copy, Default, Debug)]
struct FrameRow {
    frames: u64,
    summed: u64,
    most: u64,
}

/// The frames tasks ran in, by the task's kind, each counted when it ends with the bytes it had
/// used (its peak: a frame only grows until it is reset). A frame, not a task: a `CrateScope`
/// task resolved on the pool and then answered here ended two frames, and a file's body job is
/// one frame for all the file's `BodyCheck` tasks.
#[derive(Clone, Copy, Default, Debug)]
pub struct TaskFrames {
    kinds: [FrameRow; KINDS],
}

impl TaskFrames {
    /// A frame a task of `kind` ran in ended with `used` bytes used.
    pub(crate) fn ended(&mut self, kind: Kind, used: usize) {
        let row = &mut self.kinds[kind.index()];
        row.frames += 1;
        row.summed += used as u64;
        row.most = row.most.max(used as u64);
    }
}

impl fmt::Display for TaskFrames {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for kind in Kind::ALL {
            let row = &self.kinds[kind.index()];
            if row.frames == 0 {
                continue;
            }
            writeln!(f, "task frame bytes: {} (frames, summed, most) {} {} {}", kind.name(), row.frames, row.summed, row.most)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three records of two kinds give two rows, each with its kind's count and sum and its
    /// largest named; a smaller record after the largest does not take its name. Prints
    /// `record rows: FileItems 2 records 40 bytes, largest 30 (src/big.rs); CrateScope 1 record 20 bytes (core)`.
    #[test]
    fn records_of_two_kinds_give_two_rows_with_their_sums_and_largest() {
        let mut records = Records::new();
        records.made(Kind::FileItems, 30, || Label::of(format_args!("src/big.rs")));
        records.made(Kind::FileItems, 10, || Label::of(format_args!("src/small.rs")));
        records.made(Kind::CrateScope, 20, || Label::of(format_args!("core")));
        let text = std::format!("{records}");
        let rows: std::vec::Vec<&str> = text.lines().collect();
        assert_eq!(
            rows,
            [
                "record bytes: FileItems (records, bytes, largest: src/big.rs) 2 40 30",
                "record bytes: CrateScope (records, bytes, largest: core) 1 20 20",
            ],
            "{text}"
        );
        std::println!("record rows: FileItems 2 records 40 bytes, largest 30 (src/big.rs); CrateScope 1 record 20 bytes (core)");
    }

    /// A largest record's sections are kept with its crate's name, and every record's are
    /// summed; a label longer than [`LABEL`] is cut at a char boundary.
    #[test]
    fn scope_sections_sum_and_keep_the_largest() {
        let mut records = Records::new();
        let mut small = [0usize; SECTIONS];
        small[0] = 5;
        let mut large = [0usize; SECTIONS];
        large[0] = 7;
        large[SECTIONS - 1] = 100;
        records.scope(small, 5, || Label::of(format_args!("alloc")));
        records.scope(large, 107, || Label::of(format_args!("core")));
        let text = std::format!("{records}");
        assert!(text.contains("scope section bytes: exports (summed over the command, in the largest: core) 12 7\n"), "{text}");
        let long = "é".repeat(LABEL);
        assert_eq!(Label::of(format_args!("{long}")).as_str().len(), LABEL);
    }
}
